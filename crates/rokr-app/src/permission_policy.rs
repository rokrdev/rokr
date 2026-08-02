//! Permission mode policy layer (ticket 71).
//!
//! Extends ADR 0005 / ticket 55's headless-only [`crate::cli::PermissionMode`]
//! (`Deny` / `AcceptEdits` / `Bypass`) into a pure, TUI-free policy layer
//! usable by both drivers, per ADR 0016. [`PermissionPolicy::resolve`] sits
//! strictly upstream of `rokr_core::run_tool_loop`'s `request_permission`
//! callback: only a [`Resolution::Prompt`] outcome is meant to ever reach
//! that callback. `Allow`/`Deny` short-circuit before it, so an allowlisted
//! (or denied) action never prompts. Wiring `Resolution::Prompt` into the
//! real `request_permission` callback is ticket 72's job, not this one.

use std::collections::HashSet;
use std::path::Path;

use crate::cli::PermissionMode;

/// The outcome of evaluating a permission decision for a single tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Execute the tool call without prompting.
    Allow,
    /// Reject the tool call without prompting.
    Deny,
    /// Ask the user (via `rokr_core::run_tool_loop`'s `request_permission`
    /// callback, wired up in ticket 72).
    Prompt,
}

/// Tool-name-keyed accumulator of prior "remember for this session" grants.
///
/// Path-keyed grants are a natural follow-on this shape doesn't foreclose,
/// but this ticket only tracks grants by tool name -- see ADR 0016.
#[derive(Debug, Clone, Default)]
pub struct SessionGrants {
    granted_tools: HashSet<String>,
}

impl SessionGrants {
    /// Creates an empty set of session grants.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a prior-session grant for `tool_name`.
    pub fn grant(&mut self, tool_name: impl Into<String>) {
        self.granted_tools.insert(tool_name.into());
    }

    /// Returns true if `tool_name` has a recorded grant.
    fn is_granted(&self, tool_name: &str) -> bool {
        self.granted_tools.contains(tool_name)
    }
}

/// Pure policy layer resolving a permission decision without any TUI
/// dependency.
pub struct PermissionPolicy;

impl PermissionPolicy {
    /// Resolves whether `tool_name` should be allowed, denied, or prompted
    /// for, given the headless-equivalent `mode` and any prior session
    /// grants.
    ///
    /// Ticket 72 (`tui-session-allowlist-grant`) widened `mode` from
    /// `PermissionMode` to `Option<PermissionMode>`: the interactive TUI has
    /// no ambient permission mode at all (`--permission-mode` stays
    /// headless-only, per `crate::cli`'s doc comment), so it has nothing
    /// meaningful to pass here besides "no mode." `None` means exactly
    /// that -- absent a prior grant, it resolves to `Resolution::Prompt` for
    /// every tool, since there is no ambient mode to fall back on.
    /// `Some(mode)` preserves the original 3-mode behavior unchanged
    /// (`Bypass` -> `Allow`, `Deny` -> `Deny`, `AcceptEdits` -> `Allow` for
    /// `write`/`edit` only, `Prompt` otherwise) -- this is what headless
    /// callers pass. See `docs/adr/0016-permission-mode-policy-layer.md`'s
    /// "Amendment (ticket 72)" section for the full rationale and rejected
    /// alternatives.
    ///
    /// `path` is accepted but unused today: `SessionGrants` is tool-name
    /// keyed only in this ticket (see ADR 0016), so nothing here yet
    /// branches on it. It is part of the public signature because the
    /// ticket's acceptance criterion fixes this shape ahead of a follow-on
    /// ticket that may add path-keyed grants without another signature
    /// change.
    pub fn resolve(
        mode: Option<PermissionMode>,
        tool_name: &str,
        _path: Option<&Path>,
        grants: &SessionGrants,
    ) -> Resolution {
        if grants.is_granted(tool_name) {
            return Resolution::Allow;
        }

        match mode {
            // No ambient mode (the interactive TUI's reality): absent a
            // prior grant (checked above), always prompt.
            None => Resolution::Prompt,
            Some(PermissionMode::Bypass) => Resolution::Allow,
            Some(PermissionMode::Deny) => Resolution::Deny,
            Some(PermissionMode::AcceptEdits) => {
                if tool_name == "write" || tool_name == "edit" {
                    Resolution::Allow
                } else {
                    Resolution::Prompt
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bypass_mode_resolves_allow_for_every_tool() {
        let grants = SessionGrants::new();
        for tool in ["bash", "write", "edit", "read", "anything"] {
            assert_eq!(
                PermissionPolicy::resolve(Some(PermissionMode::Bypass), tool, None, &grants),
                Resolution::Allow,
                "tool {tool} should be allowed under Bypass mode"
            );
        }
    }

    #[test]
    fn deny_mode_resolves_deny_without_reaching_prompt() {
        let grants = SessionGrants::new();
        for tool in ["bash", "write", "edit", "read"] {
            assert_eq!(
                PermissionPolicy::resolve(Some(PermissionMode::Deny), tool, None, &grants),
                Resolution::Deny,
                "tool {tool} should be denied under Deny mode"
            );
        }
    }

    #[test]
    fn accept_edits_mode_resolves_allow_for_write_and_edit_only() {
        let grants = SessionGrants::new();
        assert_eq!(
            PermissionPolicy::resolve(Some(PermissionMode::AcceptEdits), "write", None, &grants),
            Resolution::Allow
        );
        assert_eq!(
            PermissionPolicy::resolve(Some(PermissionMode::AcceptEdits), "edit", None, &grants),
            Resolution::Allow
        );
        assert_eq!(
            PermissionPolicy::resolve(Some(PermissionMode::AcceptEdits), "bash", None, &grants),
            Resolution::Prompt
        );
        assert_eq!(
            PermissionPolicy::resolve(Some(PermissionMode::AcceptEdits), "read", None, &grants),
            Resolution::Prompt
        );
    }

    #[test]
    fn a_prior_session_grant_for_a_tool_resolves_allow_without_prompting_again() {
        let mut grants = SessionGrants::new();
        grants.grant("bash");

        // A grant overrides even Deny mode.
        assert_eq!(
            PermissionPolicy::resolve(Some(PermissionMode::Deny), "bash", None, &grants),
            Resolution::Allow
        );
        // An ungranted tool is unaffected by the grant.
        assert_eq!(
            PermissionPolicy::resolve(Some(PermissionMode::Deny), "write", None, &grants),
            Resolution::Deny
        );
    }

    #[test]
    fn no_matching_mode_or_grant_resolves_prompt() {
        let grants = SessionGrants::new();
        assert_eq!(
            PermissionPolicy::resolve(Some(PermissionMode::AcceptEdits), "bash", None, &grants),
            Resolution::Prompt
        );
    }

    /// Ticket 72: the NEW `None` semantics -- the interactive TUI has no
    /// ambient permission mode at all (see `resolve`'s doc comment), so
    /// with no prior grant, `None` must resolve to `Prompt` for every tool,
    /// same as `AcceptEdits` does for a non-write/edit tool. This is the
    /// genuinely new behavior this ticket adds; get a real RED on this
    /// (fails with a Deny/Prompt assertion mismatch against the deliberate
    /// placeholder above) before fixing the placeholder to the real value.
    #[test]
    fn none_mode_without_grant_resolves_prompt() {
        let grants = SessionGrants::new();
        for tool in ["bash", "write", "edit", "read", "anything"] {
            assert_eq!(
                PermissionPolicy::resolve(None, tool, None, &grants),
                Resolution::Prompt,
                "tool {tool} should be prompted for under None (no ambient mode)"
            );
        }
    }

    /// Ticket 72: a prior grant still overrides `None` the same way it
    /// overrides `Deny` (`a_prior_session_grant_for_a_tool_resolves_allow_
    /// without_prompting_again` above) -- precedence-check-first logic is
    /// unchanged by this ticket, so this is a regression/invariant guard,
    /// not the delta under test; it may pass immediately once the grant
    /// check itself (unchanged) runs before the `None` placeholder arm.
    #[test]
    fn a_prior_session_grant_for_a_tool_resolves_allow_under_none_mode() {
        let mut grants = SessionGrants::new();
        grants.grant("bash");

        assert_eq!(
            PermissionPolicy::resolve(None, "bash", None, &grants),
            Resolution::Allow
        );
        assert_eq!(
            PermissionPolicy::resolve(None, "write", None, &grants),
            Resolution::Prompt
        );
    }
}
