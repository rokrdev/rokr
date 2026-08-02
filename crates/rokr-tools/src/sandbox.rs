//! The `Sandbox` trait and macOS `SeatbeltSandbox` backend: pure profile-string
//! generation for a deny-by-default filesystem/network confinement, per
//! `docs/adr/0015-sandbox-trait-and-seatbelt-backend.md`. This module only
//! produces the profile text -- wrapping a real subprocess in a
//! `sandbox-exec` invocation is ticket 69's job, not this one's.

use std::path::Path;

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

#[cfg(test)]
mod tests {
    use super::{Grants, Sandbox, SeatbeltSandbox};

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
}
