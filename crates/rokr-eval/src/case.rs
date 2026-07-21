//! Ticket 58 (eval-case-runner-and-deterministic-assertions): the eval
//! case-file schema. Each case file (`*.json` under a cases dir) is a
//! prompt, a list of setup fixture files to write into the fresh temp
//! fixture dir before the headless turn runs, an agent tier, a permission
//! mode, and a list of deterministic assertions checked against the
//! fixture dir afterward.
//!
//! Assumption (see this ticket's report): case files are JSON only, parsed
//! via `serde_json` (already a workspace dependency) -- not TOML. The
//! ticket text says "TOML or JSON"; JSON-only is the simpler choice the
//! ticket brief explicitly allows, avoiding a new non-workspace `toml`
//! dependency for no behavioral gain.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// One case file's parsed contents.
#[derive(Debug, Deserialize)]
pub struct Case {
    pub prompt: String,
    /// Files written into the fresh fixture dir before the headless turn
    /// runs, relative to the fixture dir root. Absent/empty means the
    /// fixture dir starts empty.
    #[serde(default)]
    pub setup_files: Vec<SetupFile>,
    pub agent: CaseAgentTier,
    pub permission_mode: CasePermissionMode,
    pub assertions: Vec<Assertion>,
}

/// One fixture file to write before the headless turn runs.
#[derive(Debug, Deserialize)]
pub struct SetupFile {
    /// Path relative to the fixture dir root. Parent directories are
    /// created as needed.
    pub path: String,
    pub contents: String,
}

/// A case file's own spelling of the agent tier, independent of
/// `rokr_app::cli::AgentTier`'s `clap::ValueEnum` derive (which this crate
/// deliberately does not depend on parsing from -- a case file is data, not
/// a CLI flag).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseAgentTier {
    Plan,
    Build,
}

impl CaseAgentTier {
    pub fn to_agent_tier(self) -> rokr_app::AgentTier {
        match self {
            CaseAgentTier::Plan => rokr_app::AgentTier::Plan,
            CaseAgentTier::Build => rokr_app::AgentTier::Build,
        }
    }
}

/// A case file's own spelling of the permission mode. See
/// [`CaseAgentTier`]'s doc comment for why this is a separate enum from
/// `rokr_app::cli::PermissionMode` rather than reusing its `ValueEnum`
/// spelling (`accept-edits`, kebab-case) -- case files use `snake_case`
/// like every other field name in this schema.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CasePermissionMode {
    Deny,
    AcceptEdits,
    Bypass,
}

impl CasePermissionMode {
    pub fn to_permission_mode(self) -> rokr_app::PermissionMode {
        match self {
            CasePermissionMode::Deny => rokr_app::PermissionMode::Deny,
            CasePermissionMode::AcceptEdits => rokr_app::PermissionMode::AcceptEdits,
            CasePermissionMode::Bypass => rokr_app::PermissionMode::Bypass,
        }
    }
}

/// One deterministic assertion checked against the fixture dir after the
/// headless turn completes. This ticket's slice: `file_exists`,
/// `file_contains`, `git_diff`, `command_exit`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Assertion {
    FileExists {
        path: String,
    },
    FileContains {
        path: String,
        pattern: String,
    },
    GitDiff {
        expected: String,
    },
    CommandExit {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        expected_code: i32,
    },
}

/// One case file's parsed contents plus the name it's reported under (the
/// file stem, e.g. `passing-file-exists.json` -> `passing-file-exists`).
#[derive(Debug)]
pub struct LoadedCase {
    pub name: String,
    pub case: Case,
}

/// Reads every `*.json` case file directly under `cases_dir` (not
/// recursive), sorted by filename for deterministic case ordering. Returns
/// an error string on any I/O or parse failure -- `rokr eval` treats a
/// malformed case file as a hard error for the whole run rather than
/// silently skipping it.
pub fn load_cases(cases_dir: &Path) -> Result<Vec<LoadedCase>, String> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(cases_dir)
        .map_err(|err| format!("failed to read cases dir {}: {err}", cases_dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect();
    entries.sort();

    entries
        .into_iter()
        .map(|path| {
            let name = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default()
                .to_string();
            let contents = std::fs::read_to_string(&path)
                .map_err(|err| format!("failed to read case file {}: {err}", path.display()))?;
            let case: Case = serde_json::from_str(&contents)
                .map_err(|err| format!("failed to parse case file {}: {err}", path.display()))?;
            Ok(LoadedCase { name, case })
        })
        .collect()
}
