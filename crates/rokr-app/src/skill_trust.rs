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

/// One `--allow-skill` value, parsed at clap arg-parse time (ADR 0018
/// decision 4's previously-deferred CI-friendly pre-approval flag, now
/// implemented). Two forms: a bare skill name (`hash: None`) pre-approves
/// that skill regardless of its content hash; `name@<sha256-hex>` (`hash:
/// Some(..)`) pre-approves ONLY when the skill file's content hash --
/// computed the same way [`hash_skill_contents`] computes it for the trust
/// store -- matches exactly. Implements [`std::str::FromStr`] (rather than
/// a bespoke clap `value_parser`) so a malformed value is rejected by clap
/// itself, at parse time, before `Cli` is ever constructed -- see
/// `crates/rokr-app/src/cli.rs`'s `Cli::allow_skill` field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowSkillEntry {
    pub name: String,
    pub hash: Option<String>,
}

/// Whether `hash` is exactly 64 lowercase hex characters -- the same shape
/// [`hash_skill_contents`] always produces, and the only shape a hash-pinned
/// `--allow-skill name@<hash>` value's pin is accepted in (uppercase hex is
/// deliberately rejected rather than lowercased silently: a value clap
/// accepted should be unambiguous about what it means, not normalized behind
/// the operator's back).
fn is_valid_sha256_hex(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl std::str::FromStr for AllowSkillEntry {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.split_once('@') {
            Some((name, hash)) => {
                if name.is_empty() {
                    return Err(format!(
                        "--allow-skill value {value:?} has an empty skill name before '@'"
                    ));
                }
                if !is_valid_sha256_hex(hash) {
                    return Err(format!(
                        "--allow-skill value {value:?} has an invalid sha256 pin after '@' -- \
                         expected exactly 64 lowercase hex characters"
                    ));
                }
                Ok(AllowSkillEntry {
                    name: name.to_string(),
                    hash: Some(hash.to_string()),
                })
            }
            None => {
                if value.is_empty() {
                    return Err("--allow-skill value must not be empty".to_string());
                }
                Ok(AllowSkillEntry {
                    name: value.to_string(),
                    hash: None,
                })
            }
        }
    }
}

/// The result of checking a skill mention's `(name, content hash)` against a
/// [`SkillAllowlist`] -- see [`SkillAllowlist::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowlistCheck {
    /// A matching entry was found (bare name, or a pinned hash that matches
    /// exactly).
    Allowed,
    /// No unconditional match, but at least one pinned entry named this
    /// skill with a DIFFERENT hash -- ADR 0018 decision 4's `--allow-skill`
    /// note: treated as not allowed, never silently approved.
    HashMismatch,
    /// No entry names this skill at all.
    NotListed,
}

/// The parsed `--allow-skill` flag values (repeatable), consulted by a
/// [`ConsentResolver`] impl on a trust-store miss, before prompting/the
/// inert fallback -- ADR 0018 decision 4's previously-deferred flag. Never
/// consulted for a `Bypass`-mode headless run (already executes everything)
/// or a user-scope skill (already auto-trusted); see `resolve_skill_mentions
/// _with_consent`'s `SkillScope::User` arm in `crate::commands`, which never
/// even constructs a [`SkillConsentRequest`] for that case.
#[derive(Debug, Clone, Default)]
pub struct SkillAllowlist {
    entries: Vec<AllowSkillEntry>,
}

impl SkillAllowlist {
    pub fn new(entries: Vec<AllowSkillEntry>) -> Self {
        Self { entries }
    }

    /// Checks `name`/`hash` (a skill mention's name and its file's current
    /// content hash) against every entry. Scans the full list rather than
    /// short-circuiting on the first name match, so a mismatched pinned
    /// entry earlier in the list doesn't shadow a later unconditional or
    /// correctly-pinned entry for the same name.
    pub fn check(&self, name: &str, hash: &str) -> AllowlistCheck {
        let mut hash_mismatch = false;
        for entry in &self.entries {
            if entry.name != name {
                continue;
            }
            match &entry.hash {
                None => return AllowlistCheck::Allowed,
                Some(pinned) if pinned == hash => return AllowlistCheck::Allowed,
                Some(_) => hash_mismatch = true,
            }
        }
        if hash_mismatch {
            AllowlistCheck::HashMismatch
        } else {
            AllowlistCheck::NotListed
        }
    }
}

/// Whether an [`AllowlistCheck`] short-circuits consent before a
/// [`ConsentResolver`] ever builds/shows a real prompt -- pulled out as a
/// pure function (mirroring [`map_permission_decision`]'s precedent) so it's
/// directly unit-testable without a live `rokr_tui::PermissionHandle` (which
/// only `rokr_tui` itself can construct -- see `InteractiveConsentResolver`'s
/// doc comment). `Some(outcome)` means `resolve` returns immediately,
/// consulting neither the trust store's usual TOFU prompt path nor writing
/// any trust-store entry (the approval is ephemeral, same spirit as "[y] run
/// once"). `None` means normal flow proceeds -- a `HashMismatch` additionally
/// gets a one-line stderr notice from the caller before falling through.
fn allowlist_short_circuit(check: AllowlistCheck) -> Option<ConsentOutcome> {
    match check {
        AllowlistCheck::Allowed => Some(ConsentOutcome::ApproveWithoutPersisting),
        AllowlistCheck::HashMismatch | AllowlistCheck::NotListed => None,
    }
}

/// What the consent prompt shows: the literal `run:` command, the skill's
/// path, and its scope -- ADR 0018 decision 7, no dry-run, no output
/// preview. `name` and `hash` (added for `--allow-skill`, ADR 0018 decision
/// 4's previously-deferred flag) are the same mention name and content hash
/// `resolve_skill_mentions_with_consent` already computed before building
/// this request -- carried here so a [`ConsentResolver`] impl can check a
/// [`SkillAllowlist`] itself, entirely inside the consent-resolution seam,
/// without `commands.rs` needing any allowlist-awareness of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillConsentRequest {
    pub command: String,
    pub skill_path: PathBuf,
    pub scope: SkillScope,
    pub name: String,
    pub hash: String,
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
/// new prompt variant. `allowlist` defaults empty (`new`); a caller wires in
/// the parsed `--allow-skill` flag values via [`Self::with_allowlist`].
#[derive(Clone)]
pub struct InteractiveConsentResolver {
    handle: rokr_tui::PermissionHandle,
    allowlist: SkillAllowlist,
}

impl InteractiveConsentResolver {
    pub fn new(handle: rokr_tui::PermissionHandle) -> Self {
        Self {
            handle,
            allowlist: SkillAllowlist::default(),
        }
    }

    /// ADR 0018 decision 4's `--allow-skill` flag (deferred there,
    /// implemented here): a matching entry short-circuits [`Self::resolve`]
    /// before it ever builds/shows the interactive prompt -- see
    /// `allowlist_short_circuit`.
    pub fn with_allowlist(mut self, allowlist: SkillAllowlist) -> Self {
        self.allowlist = allowlist;
        self
    }
}

/// Builds the literal text `InteractiveConsentResolver::resolve` shows via
/// `PermissionDetail::Text` -- pulled out as its own function (F-001,
/// review) so it's unit-testable without a live `rokr_tui::PermissionHandle`.
/// The TUI holds the terminal in raw mode plus the alternate screen, so a
/// bare `eprintln!` (headless's approach, via `notice_sink`) would be
/// garbled or simply invisible on this path -- the hash-mismatch note has to
/// travel IN-BAND, as the first line of the same detail text the prompt
/// already renders, or the user has no way to see why they're being
/// prompted despite having passed `--allow-skill`.
fn build_skill_consent_detail(request: &SkillConsentRequest, hash_mismatch: bool) -> String {
    let mut detail = String::new();
    if hash_mismatch {
        detail.push_str(&format!(
            "note: --allow-skill pin for '{}' did not match this file's current content hash\n",
            request.name
        ));
    }
    detail.push_str(&format!(
        "run: {}\nskill: {}\nscope: {}",
        request.command,
        request.skill_path.display(),
        request.scope,
    ));
    detail
}

impl ConsentResolver for InteractiveConsentResolver {
    fn resolve(
        &self,
        request: SkillConsentRequest,
    ) -> impl std::future::Future<Output = ConsentOutcome> + Send {
        let handle = self.handle.clone();
        let allowlist = self.allowlist.clone();
        async move {
            let check = allowlist.check(&request.name, &request.hash);
            if let Some(outcome) = allowlist_short_circuit(check) {
                return outcome;
            }
            let detail =
                build_skill_consent_detail(&request, check == AllowlistCheck::HashMismatch);
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
    allowlist: SkillAllowlist,
    notice_sink: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
}

impl HeadlessConsentResolver {
    pub fn new(mode: crate::cli::PermissionMode) -> Self {
        Self {
            mode,
            allowlist: SkillAllowlist::default(),
            notice_sink: std::sync::Arc::new(|notice: &str| eprintln!("{notice}")),
        }
    }

    /// ADR 0018 decision 4's `--allow-skill` flag (deferred there,
    /// implemented here): a matching entry short-circuits [`Self::resolve`]
    /// with no prompt/notice and no trust-store write -- see
    /// `allowlist_short_circuit`. Chainable with [`Self::with_notice_sink`].
    pub fn with_allowlist(mut self, allowlist: SkillAllowlist) -> Self {
        self.allowlist = allowlist;
        self
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
            allowlist: SkillAllowlist::default(),
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
        let allowlist = self.allowlist.clone();
        let notice_sink = self.notice_sink.clone();
        async move {
            if matches!(mode, crate::cli::PermissionMode::Bypass) {
                return ConsentOutcome::ApproveWithoutPersisting;
            }
            let check = allowlist.check(&request.name, &request.hash);
            if let Some(outcome) = allowlist_short_circuit(check) {
                return outcome;
            }
            if check == AllowlistCheck::HashMismatch {
                notice_sink(&format!(
                    "skill '{}': --allow-skill hash pin did not match this file's current \
                     content hash",
                    request.name
                ));
            }
            notice_sink(&format!(
                "skill not executed (untrusted): {} [run: {}]",
                request.skill_path.display(),
                request.command,
            ));
            ConsentOutcome::Decline
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

    // ---------------------------------------------------------------
    // `--allow-skill` (ADR 0018 decision 4, previously deferred).
    // ---------------------------------------------------------------

    /// A bare name and a `name@<64-hex>` value both parse; a hash pin that
    /// is the wrong length, contains non-hex or uppercase-hex characters, or
    /// an empty name before `@`, are all rejected -- matching
    /// `hash_skill_contents`'s always-lowercase-64-hex output shape exactly.
    #[test]
    fn allow_skill_entry_parses_bare_name_and_name_at_hash_rejects_malformed() {
        let entry: AllowSkillEntry = "deploy".parse().expect("bare name should parse");
        assert_eq!(entry.name, "deploy");
        assert_eq!(entry.hash, None);

        let hash = hash_skill_contents("run: echo hi");
        let entry: AllowSkillEntry = format!("deploy@{hash}")
            .parse()
            .expect("name@<64-hex> should parse");
        assert_eq!(entry.name, "deploy");
        assert_eq!(entry.hash, Some(hash));

        assert!(
            "deploy@abc123".parse::<AllowSkillEntry>().is_err(),
            "a hash pin shorter than 64 hex chars must be rejected"
        );
        assert!(
            format!("deploy@{}", "a".repeat(63))
                .parse::<AllowSkillEntry>()
                .is_err(),
            "a 63-char pin must be rejected"
        );
        assert!(
            format!("deploy@{}", "g".repeat(64))
                .parse::<AllowSkillEntry>()
                .is_err(),
            "a pin containing non-hex characters must be rejected"
        );
        assert!(
            format!("deploy@{}", "A".repeat(64))
                .parse::<AllowSkillEntry>()
                .is_err(),
            "an uppercase-hex pin must be rejected -- only lowercase hex is accepted"
        );
        assert!(
            "@abc".parse::<AllowSkillEntry>().is_err(),
            "an empty skill name before '@' must be rejected"
        );
        assert!(
            "".parse::<AllowSkillEntry>().is_err(),
            "an empty bare value must be rejected"
        );
    }

    /// `SkillAllowlist::check`: an unconditional (bare-name) entry always
    /// matches; a pinned entry matches only the exact hash; a name not
    /// present at all is `NotListed`; a pinned entry present under the
    /// wrong hash is `HashMismatch`, not silently allowed.
    #[test]
    fn skill_allowlist_check_distinguishes_allowed_mismatch_and_not_listed() {
        let hash_a = hash_skill_contents("run: echo original");
        let hash_b = hash_skill_contents("run: echo edited");
        assert_ne!(hash_a, hash_b, "fixture sanity");

        let allowlist = SkillAllowlist::new(vec![
            AllowSkillEntry {
                name: "deploy".to_string(),
                hash: None,
            },
            AllowSkillEntry {
                name: "release".to_string(),
                hash: Some(hash_a.clone()),
            },
        ]);

        assert_eq!(
            allowlist.check("deploy", &hash_b),
            AllowlistCheck::Allowed,
            "a bare-name entry must match regardless of hash"
        );
        assert_eq!(
            allowlist.check("release", &hash_a),
            AllowlistCheck::Allowed,
            "a pinned entry must match its exact hash"
        );
        assert_eq!(
            allowlist.check("release", &hash_b),
            AllowlistCheck::HashMismatch,
            "a pinned entry present under the WRONG hash must be HashMismatch, never Allowed"
        );
        assert_eq!(
            allowlist.check("unknown", &hash_a),
            AllowlistCheck::NotListed,
            "a name with no entry at all must be NotListed"
        );
    }

    /// F-002 (review): `SkillAllowlist::check`'s doc comment promises a
    /// mismatched pinned entry earlier in the list never shadows a later
    /// matching entry for the SAME name -- the full list is always scanned,
    /// not short-circuited on the first name match. Covers all three
    /// multi-entry shapes: a wrong pin followed by the right pin, a wrong
    /// pin followed by an unconditional bare-name entry, and two different
    /// wrong pins with no matching entry at all.
    #[test]
    fn skill_allowlist_check_scans_full_list_for_same_name_entries() {
        let wrong_hash_a = hash_skill_contents("run: echo wrong a");
        let wrong_hash_b = hash_skill_contents("run: echo wrong b");
        let right_hash = hash_skill_contents("run: echo right");

        let allowlist = SkillAllowlist::new(vec![
            AllowSkillEntry {
                name: "deploy".to_string(),
                hash: Some(wrong_hash_a.clone()),
            },
            AllowSkillEntry {
                name: "deploy".to_string(),
                hash: Some(right_hash.clone()),
            },
        ]);
        assert_eq!(
            allowlist.check("deploy", &right_hash),
            AllowlistCheck::Allowed,
            "a wrong pin earlier in the list must not shadow a later CORRECT pin for the same \
             name"
        );

        let allowlist = SkillAllowlist::new(vec![
            AllowSkillEntry {
                name: "deploy".to_string(),
                hash: Some(wrong_hash_a.clone()),
            },
            AllowSkillEntry {
                name: "deploy".to_string(),
                hash: None,
            },
        ]);
        assert_eq!(
            allowlist.check("deploy", &wrong_hash_b),
            AllowlistCheck::Allowed,
            "a wrong pin earlier in the list must not shadow a later UNCONDITIONAL (bare-name) \
             entry for the same name"
        );

        let allowlist = SkillAllowlist::new(vec![
            AllowSkillEntry {
                name: "deploy".to_string(),
                hash: Some(wrong_hash_a.clone()),
            },
            AllowSkillEntry {
                name: "deploy".to_string(),
                hash: Some(wrong_hash_b.clone()),
            },
        ]);
        assert_eq!(
            allowlist.check("deploy", &right_hash),
            AllowlistCheck::HashMismatch,
            "two different wrong pins for the same name, with no entry matching the actual \
             hash, must still be HashMismatch, not NotListed"
        );
    }

    /// F-001 (review): the TUI holds the terminal in raw mode plus the
    /// alternate screen, so a bare `eprintln!` on hash-mismatch fallback
    /// (headless's approach) would be garbled or invisible here -- the note
    /// must instead be the first line of the SAME `PermissionDetail::Text`
    /// the consent prompt already renders, so the user can see why they're
    /// being prompted despite having passed `--allow-skill`.
    #[test]
    fn skill_consent_detail_prepends_mismatch_note_only_when_hash_mismatched() {
        let request = SkillConsentRequest {
            command: "echo hi".to_string(),
            skill_path: PathBuf::from("/fake/.rokr/skills/deploy.md"),
            scope: SkillScope::Project,
            name: "deploy".to_string(),
            hash: hash_skill_contents("run: echo hi"),
        };

        let detail = build_skill_consent_detail(&request, true);
        assert!(
            detail.starts_with("note: --allow-skill pin for 'deploy' did not match"),
            "expected the mismatch note as the FIRST line of the detail text, got: {detail:?}"
        );
        assert!(
            detail.contains("run: echo hi") && detail.contains("skill: "),
            "expected the usual run:/skill:/scope: fields to still be present, got: {detail:?}"
        );

        let detail = build_skill_consent_detail(&request, false);
        assert!(
            !detail.contains("note:"),
            "expected no mismatch note when there was no hash mismatch, got: {detail:?}"
        );
        assert!(detail.starts_with("run: echo hi"));
    }

    /// Unit test for the allowlist short-circuit `InteractiveConsentResolver
    /// ::resolve` applies BEFORE it ever builds/shows the interactive
    /// prompt or touches its `rokr_tui::PermissionHandle` -- exercised at
    /// the pure-function level (`allowlist_short_circuit`, the exact
    /// function `resolve`'s first branch calls) since
    /// `rokr_tui::PermissionHandle` has no public constructor outside
    /// `rokr_tui` itself (see `InteractiveConsentResolver`'s doc comment
    /// and `crate::runner`'s identical precedent for why a live handle
    /// can't be constructed in this crate's tests) -- a full TUI
    /// integration test is not needed to prove the short-circuit logic
    /// itself is correct.
    #[test]
    fn allowlist_short_circuit_skips_prompt_only_on_an_unconditional_or_matching_pin() {
        assert_eq!(
            allowlist_short_circuit(AllowlistCheck::Allowed),
            Some(ConsentOutcome::ApproveWithoutPersisting),
            "an allowed check must short-circuit straight to a one-shot, never-persisted approval"
        );
        assert_eq!(
            allowlist_short_circuit(AllowlistCheck::HashMismatch),
            None,
            "a mismatched pin must fall through to the normal prompt flow, not short-circuit"
        );
        assert_eq!(
            allowlist_short_circuit(AllowlistCheck::NotListed),
            None,
            "an unlisted skill must fall through to the normal prompt flow"
        );
    }
}
