//! Ticket 75 (executable-skill-invocation), ADR 0018
//! (`docs/adr/0018-executable-skill-trust-model.md`): the TOFU
//! (trust-on-first-use) hash-pinned trust store for executable skills' `run:`
//! frontmatter commands, and the `ConsentResolver` seam
//! `CommandRegistry::resolve_skills` (`crate::commands`) consults before
//! ever executing one.
//!
//! [`SkillTrustStore`] persists `(absolute_skill_path, sha256(skill_file_contents))`
//! pairs to a single JSON file under the caller-supplied config dir --
//! shaped after `rokr-provider::auth::FileTokenStore`'s load/save-over-JSON
//! pattern (ADR 0018 decision 5's explicit precedent). It is USER-SCOPE ONLY
//! by construction: every constructor call site in this codebase passes
//! `rokr_config::default_config_dir()` (or, in tests, an isolated tempdir
//! standing in for it) -- there is no method anywhere in this module that
//! accepts or derives a project-scope path, because ADR 0018 decision 5 is
//! explicit that no such file exists, ever: a project-scope skill can never
//! self-certify its own execution.
//!
//! [`ConsentResolver`] mirrors `crate::runner::PermissionRequester`'s shape
//! (a trait abstracting interactive TUI vs. non-interactive headless
//! dispatch) -- see ADR 0018 decision 3.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Computes the hex-encoded SHA-256 digest of `contents`. Deliberately takes
/// the exact in-memory string a caller is about to act on (never re-reads
/// the file from disk) -- see [`SkillTrustStore`]'s doc comment and ADR 0018
/// decision 1's TOCTOU note: the hash must be over "what actually runs," not
/// a value re-derived after the trust decision was made.
pub fn hash_skill_contents(contents: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(contents.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// One granted `(path, content-hash)` pair.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct TrustedSkill {
    path: PathBuf,
    hash: String,
}

/// The user-scope-only TOFU trust store (ADR 0018 decisions 1 and 5). See
/// this module's doc comment for the user-scope-only guarantee.
#[derive(Clone)]
pub struct SkillTrustStore {
    path: PathBuf,
}

/// Errors persisting/loading the trust store's backing JSON file.
#[derive(Debug, thiserror::Error)]
pub enum SkillTrustError {
    #[error("skill trust store io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("skill trust store serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

impl SkillTrustStore {
    /// `config_dir` MUST be the user's own rokr config directory
    /// (`rokr_config::default_config_dir()` in production) -- never a
    /// project directory; see this module's doc comment.
    pub fn new(config_dir: &Path) -> Self {
        Self {
            path: config_dir.join("skill_trust.json"),
        }
    }

    /// Whether `(skill_path, hash)` has a prior trust grant recorded.
    pub fn is_trusted(&self, skill_path: &Path, hash: &str) -> bool {
        self.load()
            .iter()
            .any(|entry| entry.path == skill_path && entry.hash == hash)
    }

    /// Records a trust grant for `(skill_path, hash)`, persisting it to
    /// disk. Idempotent -- granting an already-granted pair is a no-op
    /// write, not a duplicate entry.
    pub fn grant(&self, skill_path: &Path, hash: &str) -> Result<(), SkillTrustError> {
        let mut entries = self.load();
        let key = TrustedSkill {
            path: skill_path.to_path_buf(),
            hash: hash.to_string(),
        };
        if !entries.contains(&key) {
            entries.push(key);
        }
        self.save(&entries)
    }

    /// Tolerant load: a missing or corrupt file is an empty set (a fresh
    /// user has no trust file yet), not an error -- mirrors
    /// `scan_skills_dir`'s tolerant-absence contract elsewhere in this
    /// crate.
    fn load(&self) -> Vec<TrustedSkill> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|contents| serde_json::from_str(&contents).ok())
            .unwrap_or_default()
    }

    fn save(&self, entries: &[TrustedSkill]) -> Result<(), SkillTrustError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string(entries)?;

        // M-2 (pre-ship review): hardened to 0600, mirroring
        // `rokr-provider::auth::FileTokenStore::save`'s exact precedent --
        // this file is consent history (which project-scope skill
        // versions the user has trusted to execute arbitrary commands),
        // sensitive enough to not leave world/group-readable even
        // momentarily. Unix: create via `OpenOptions` with `mode(0o600)`
        // so it's never written at a broader default mode first;
        // `set_permissions` below still runs unconditionally afterward to
        // narrow an already-existing file from a pre-fix version of this
        // code (`mode()` only governs permissions at creation time).
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;

            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&self.path)?;
            file.write_all(json.as_bytes())?;
        }

        #[cfg(not(unix))]
        {
            std::fs::write(&self.path, &json)?;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
        }

        Ok(())
    }
}

/// Which scope a discovered skill came from -- see
/// `crate::commands::CommandRegistry::discover_user_scope`/
/// `discover_project_scope`. Only `Project`-scope executable skills are ever
/// TOFU-gated (ADR 0018 decision 5); `User`-scope ones are auto-trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    User,
    Project,
}

impl std::fmt::Display for SkillScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SkillScope::User => "user",
            SkillScope::Project => "project",
        })
    }
}

/// What the consent prompt shows: the literal `run:` command, the skill's
/// path, and its scope -- ADR 0018 decision 7, no dry-run, no output
/// preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillConsentRequest {
    pub command: String,
    pub skill_path: PathBuf,
    pub scope: SkillScope,
}

/// The result of asking a [`ConsentResolver`] about a [`SkillConsentRequest`].
/// A three-way (not boolean) outcome specifically to express ADR 0018
/// decision 4's `Bypass` rule: `Bypass` executes but must NEVER write a
/// trust-store entry ("bypassing does not fabricate consent history"),
/// which a plain `bool` can't distinguish from an interactively-granted,
/// persisted approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentOutcome {
    /// Execute, and record a trust-store grant for this `(path, hash)`.
    ApproveAndPersist,
    /// Execute this one time without ever writing a trust-store entry.
    ApproveWithoutPersisting,
    /// Do not execute.
    Decline,
}

/// Abstracts interactive (TUI) vs. non-interactive (headless) consent
/// dispatch -- mirrors `crate::runner::PermissionRequester`'s shape (ADR
/// 0018 decision 3).
pub trait ConsentResolver: Send + Sync {
    fn resolve(
        &self,
        request: SkillConsentRequest,
    ) -> impl std::future::Future<Output = ConsentOutcome> + Send;
}

/// Interactive `ConsentResolver`, backed by the TUI's per-submission
/// `rokr_tui::PermissionHandle` (ADR 0018 decision 3): reuses
/// `rokr_tui::PermissionDetail::Text` to show the literal `run:` command,
/// the skill's path, and its scope (decision 7) rather than introducing a
/// new prompt variant.
#[derive(Clone)]
pub struct InteractiveConsentResolver {
    handle: rokr_tui::PermissionHandle,
}

impl InteractiveConsentResolver {
    pub fn new(handle: rokr_tui::PermissionHandle) -> Self {
        Self { handle }
    }
}

impl ConsentResolver for InteractiveConsentResolver {
    fn resolve(
        &self,
        request: SkillConsentRequest,
    ) -> impl std::future::Future<Output = ConsentOutcome> + Send {
        let handle = self.handle.clone();
        async move {
            let detail = format!(
                "run: {}\nskill: {}\nscope: {}",
                request.command,
                request.skill_path.display(),
                request.scope,
            );
            let decision = handle
                .request(rokr_tui::PermissionRequest {
                    tool_name: "skill".to_string(),
                    detail: rokr_tui::PermissionDetail::Text(detail),
                })
                .await;
            map_permission_decision(decision)
        }
    }
}

/// Maps the TUI's raw `[y]/[n]/[r]` keypress intent to a skill consent
/// outcome. Split out of [`InteractiveConsentResolver::resolve`] as a pure
/// function so the mapping itself -- the thing F-002 (pre-ship review)
/// found wrong -- is directly unit-testable without a live
/// `rokr_tui::PermissionHandle` (which requires a running render loop; see
/// this module's tests).
///
/// F-002 (pre-ship review): `Allow` and `AllowAndRemember` previously both
/// mapped to `ApproveAndPersist`, so the "[y] allow once" hint rendered by
/// rokr-tui's `permission_hint_line` was a lie -- pressing `y` wrote a
/// permanent `SkillTrustStore` grant identical to `r`, and
/// `ApproveWithoutPersisting` was unreachable interactively.
/// `ApproveWithoutPersisting` exists precisely so a skill's `run:` command
/// can execute once without being durably trusted (ADR 0018 decision 1:
/// consent is TOFU hash-pinned and a trust-store entry is written only on
/// an actual "trust this" decision, not on every execution) -- `y` now
/// maps to it, matching rokr-tui's "run once" label for a skill-consent
/// prompt, and only `r` ("trust this skill version") writes a grant.
fn map_permission_decision(decision: rokr_tui::PermissionDecision) -> ConsentOutcome {
    match decision {
        rokr_tui::PermissionDecision::Allow => ConsentOutcome::ApproveWithoutPersisting,
        rokr_tui::PermissionDecision::AllowAndRemember => ConsentOutcome::ApproveAndPersist,
        rokr_tui::PermissionDecision::Deny => ConsentOutcome::Decline,
    }
}

/// Non-interactive `ConsentResolver`, dispatching on the existing
/// `crate::cli::PermissionMode` (ADR 0018 decision 4 -- "the same concept
/// ADR 0016 already established for gated tool calls, not a parallel notion
/// invented for skills"). There is no human to ask in headless mode, so
/// every mode except `Bypass` declines: `Deny` (the default) and
/// `AcceptEdits` (which only ever grants write/edit tool calls, not a
/// skill's `run:` command) both decline, printing a one-line stderr notice
/// first.
///
/// F-003 (pre-ship review): the notice is emitted through `notice_sink`
/// rather than a bare `eprintln!`, so a test can capture and assert its
/// exact text in-process (`with_notice_sink`) instead of the only other
/// option being spawning the real `rokr` binary and inspecting its real
/// stderr.
pub struct HeadlessConsentResolver {
    mode: crate::cli::PermissionMode,
    notice_sink: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
}

impl HeadlessConsentResolver {
    pub fn new(mode: crate::cli::PermissionMode) -> Self {
        Self {
            mode,
            notice_sink: std::sync::Arc::new(|notice: &str| eprintln!("{notice}")),
        }
    }

    /// F-003 (pre-ship review): test-only seam so the exact one-line notice
    /// `resolve` prints on decline can be captured and asserted in-process
    /// -- the plain `eprintln!` in the default constructor above writes to
    /// the real process stderr, which is only observable by spawning the
    /// actual `rokr` binary (see `crates/rokr/tests/headless_test.rs`).
    #[cfg(test)]
    pub fn with_notice_sink(
        mode: crate::cli::PermissionMode,
        sink: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        Self {
            mode,
            notice_sink: std::sync::Arc::new(sink),
        }
    }
}

impl ConsentResolver for HeadlessConsentResolver {
    fn resolve(
        &self,
        request: SkillConsentRequest,
    ) -> impl std::future::Future<Output = ConsentOutcome> + Send {
        let mode = self.mode;
        let notice_sink = self.notice_sink.clone();
        async move {
            if matches!(mode, crate::cli::PermissionMode::Bypass) {
                ConsentOutcome::ApproveWithoutPersisting
            } else {
                notice_sink(&format!(
                    "skill not executed (untrusted): {} [run: {}]",
                    request.skill_path.display(),
                    request.command,
                ));
                ConsentOutcome::Decline
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load-bearing (ADR 0018 decision 1's TOCTOU closure): a trust grant
    /// recorded for a skill file's contents at hash A must NOT match a
    /// lookup for the SAME PATH once the file's contents (and therefore
    /// hash) have changed -- editing the skill re-triggers consent on next
    /// invocation, by design.
    #[test]
    fn trust_grant_is_invalidated_when_skill_file_contents_change() {
        let config_dir = tempfile::tempdir().unwrap();
        let store = SkillTrustStore::new(config_dir.path());
        let skill_path = PathBuf::from("/fake/.rokr/skills/deploy.md");

        let hash_a = hash_skill_contents("run: echo original");
        let hash_b = hash_skill_contents("run: echo edited");
        assert_ne!(
            hash_a, hash_b,
            "test fixture sanity: two different contents must hash differently"
        );

        store
            .grant(&skill_path, &hash_a)
            .expect("grant should succeed");

        assert!(
            store.is_trusted(&skill_path, &hash_a),
            "expected the exact granted (path, hash) pair to be trusted"
        );
        assert!(
            !store.is_trusted(&skill_path, &hash_b),
            "expected a lookup for the SAME PATH with a DIFFERENT hash (simulating an edited \
             skill file) to be untrusted -- a stored grant must be pinned to the exact content \
             hash it was granted for, not just the path"
        );
    }

    /// ADR 0018 decision 5: "The trust store is user-scope only and is never
    /// consulted for a project-scope trust file, because no such file
    /// exists." Grants recorded via a store rooted at a user-scope-style
    /// config dir must never write anything under a separate project
    /// directory, and nothing under the project directory should ever
    /// become a source of trust data this module reads.
    #[test]
    fn project_scope_trust_file_is_never_consulted() {
        let user_config_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let skill_path = project_dir.path().join(".rokr/skills/deploy.md");
        let hash = hash_skill_contents("run: rm -rf /");

        let user_store = SkillTrustStore::new(user_config_dir.path());
        user_store
            .grant(&skill_path, &hash)
            .expect("grant against the user-scope store should succeed");

        assert!(
            user_store.is_trusted(&skill_path, &hash),
            "expected the grant to be visible from the real user-scope store"
        );

        // Nothing was ever written anywhere under the PROJECT directory --
        // there is no project-scope trust file for this module to have
        // created, consulted, or fallen back to.
        let project_dir_is_untouched = std::fs::read_dir(project_dir.path())
            .expect("project_dir should still exist")
            .next()
            .is_none();
        assert!(
            project_dir_is_untouched,
            "expected NO file to have been written under the project directory -- the trust \
             store must never read or write a project-scope location"
        );

        // A store constructed AT the project directory (misused as if it
        // were a config dir) sees nothing -- proving the grant above landed
        // only under the real user-scope config dir, not anywhere globally
        // shared or project-reachable.
        let store_rooted_at_project_dir = SkillTrustStore::new(project_dir.path());
        assert!(
            !store_rooted_at_project_dir.is_trusted(&skill_path, &hash),
            "expected a store rooted at the project directory to see no trust data at all"
        );
    }

    /// M-2 (pre-ship review): `SkillTrustStore::save` must never leave
    /// `skill_trust.json` on disk with permissions broader than `0600` --
    /// it records consent history (which project-scope skill versions the
    /// user has trusted to execute arbitrary commands), matching the
    /// precedent `rokr-provider::auth::FileTokenStore::save` already
    /// establishes for token files.
    #[test]
    #[cfg(unix)]
    fn grant_writes_trust_store_file_at_0600() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let store = SkillTrustStore::new(temp.path());
        let skill_path = temp.path().join("deploy.md");

        store
            .grant(&skill_path, &hash_skill_contents("run: echo hi"))
            .expect("grant should succeed");

        let metadata = std::fs::metadata(temp.path().join("skill_trust.json")).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "expected skill_trust.json permissions to be 0600, got {mode:o}"
        );
    }

    /// F-002 (major, pre-ship review): `InteractiveConsentResolver` used to
    /// map BOTH `Allow` and `AllowAndRemember` to `ApproveAndPersist`, so
    /// the "[y] allow once" hint line was a lie -- pressing `y` wrote a
    /// permanent trust-store grant, identical to `r`, and
    /// `ApproveWithoutPersisting` was unreachable interactively. The
    /// resolver's mapping must match what rokr-tui's hint line tells the
    /// user each key does: `y` = run once (never persisted), `r` = trust
    /// this skill version (persisted), `n`/deny = don't run.
    #[test]
    fn permission_decision_mapping_matches_hint_line_semantics() {
        assert_eq!(
            map_permission_decision(rokr_tui::PermissionDecision::Allow),
            ConsentOutcome::ApproveWithoutPersisting,
            "'[y] run once' must never write a trust-store grant"
        );
        assert_eq!(
            map_permission_decision(rokr_tui::PermissionDecision::AllowAndRemember),
            ConsentOutcome::ApproveAndPersist,
            "'[r] trust this skill version' must be the ONLY interactive path that writes a \
             trust-store grant"
        );
        assert_eq!(
            map_permission_decision(rokr_tui::PermissionDecision::Deny),
            ConsentOutcome::Decline
        );
    }
}
