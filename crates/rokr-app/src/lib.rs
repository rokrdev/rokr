//! Ticket 52 (clap-and-sessionrunner-extraction): the `rokr-app` library
//! crate holds the two things lifted out of `crates/rokr/src/main.rs`'s
//! closures -- the `clap` (derive) CLI surface ([`cli`]) that replaced the
//! hand-rolled arg matching, and the [`runner::SessionRunner`] that owns the
//! submit-and-run orchestration the `submit` closure used to run inline. The
//! `rokr` binary depends on this crate and delegates to both; `rokr-tui`
//! stays decoupled from the message/provider model (it takes closures), so
//! there is no dependency cycle.

pub mod cli;
pub mod headless;
pub mod runner;
pub mod subagent;

pub use cli::{AgentTier, AuthAction, Cli, Command, ResumeMode};
pub use headless::{select_mode, DenyAllPermissions, Mode};
pub use runner::{
    accumulate_user_turn, append_compaction_record, capture_checkpoint_if_granted_diff,
    format_tool_call_permission_text, log_observational_hook_outcome, matching_hook_entries,
    now_timestamp, run_hook_entry, PermissionRequester, SessionRunner, SharedProvider,
    COMPACTION_SUMMARY_WRAPPER_PREFIX,
};
