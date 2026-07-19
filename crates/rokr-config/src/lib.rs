//! JSON configuration loading, schema versioning, and migrations.

use std::path::{Path, PathBuf};

/// The on-disk config schema. See docs/adr/0002-config-format-and-versioning.md
/// and docs/adr/0010-config-additive-fields-vs-version-bump.md (additive-
/// optional fields via `serde(default)`, no version bump, never written back
/// to an existing file).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Config {
    pub version: u32,
    /// Token budget used to decide when auto-compaction should trigger.
    /// Additive-optional (ADR 0010): an existing file missing this field
    /// gets the runtime default; the field is never written back.
    #[serde(default = "default_context_window_size")]
    pub context_window_size: u32,
    /// Fraction of `context_window_size` that triggers auto-compaction.
    /// Additive-optional (ADR 0010): an existing file missing this field
    /// gets the runtime default; the field is never written back.
    #[serde(default = "default_auto_compact_threshold")]
    pub auto_compact_threshold: f64,
}

fn default_context_window_size() -> u32 {
    200_000
}

fn default_auto_compact_threshold() -> f64 {
    0.7
}

/// Errors returned while loading, validating, or initializing config.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read or write config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("config file at {path} is not valid: {source}")]
    Invalid {
        path: std::path::PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("config file at {path} has unsupported version {found} (supported: {supported})")]
    UnsupportedVersion {
        path: std::path::PathBuf,
        found: u32,
        supported: u32,
    },
}

/// Default system prompt for the Plan agent, scaffolded to `agents/plan.md`
/// on first run.
const DEFAULT_PLAN_PROMPT: &str = "\
# Plan Agent

You are the Plan agent. Given a user request, analyze the relevant code and \
produce a clear, step-by-step implementation plan.

- Read enough of the codebase to understand the current behavior and \
  conventions before proposing changes.
- Break the work into small, ordered steps that a Build agent can follow \
  without further clarification.
- Call out risks, open questions, and files you expect to touch.
- Do not edit any files or write code yourself — your output is the plan.
";

/// Default system prompt for the Build agent, scaffolded to `agents/build.md`
/// on first run.
const DEFAULT_BUILD_PROMPT: &str = "\
# Build Agent

You are the Build agent. Given an implementation plan, carry it out.

- Follow the plan's steps in order, adapting only when the plan conflicts \
  with what you find in the code.
- Write tests alongside (or before) the code they cover, and keep the \
  change scoped to what the plan describes.
- Prefer small, reviewable edits over large rewrites.
- Run the relevant tests before considering a step complete.
";

/// Scaffold `agents/plan.md` and `agents/build.md` under `config_dir` with
/// default prompt content, if they do not already exist. Existing files are
/// left untouched.
fn scaffold_agent_prompts(config_dir: &Path) -> Result<(), ConfigError> {
    let agents_dir = config_dir.join("agents");
    std::fs::create_dir_all(&agents_dir)?;

    for (name, default_content) in [
        ("plan.md", DEFAULT_PLAN_PROMPT),
        ("build.md", DEFAULT_BUILD_PROMPT),
    ] {
        let path = agents_dir.join(name);
        if !path.exists() {
            std::fs::write(&path, default_content)?;
        }
    }

    Ok(())
}

/// Read the scaffolded system prompt for `agent` (e.g. `"plan"` or
/// `"build"`) from `{config_dir}/agents/{agent}.md`.
pub fn read_agent_prompt(config_dir: &Path, agent: &str) -> Result<String, ConfigError> {
    let path = config_dir.join("agents").join(format!("{agent}.md"));
    let contents = std::fs::read_to_string(path)?;
    Ok(contents)
}

/// Reads project-level context from `cwd`, if any is present. Looks for
/// `AGENTS.md` first; if it isn't there, falls back to `CLAUDE.md`. Never
/// reads both. Neither present is not an error — just no project context.
/// The fallback only triggers when `AGENTS.md` is absent (`NotFound`); a
/// read error for any other reason (e.g. a permissions error, or the path
/// existing but not being a readable file) is treated as no context,
/// without falling back to `CLAUDE.md`.
/// This is a one-time, unconditional, side-effect-free read intended to be
/// folded into the system prompt at startup — not a tool, not permission-
/// gated.
pub fn load_project_context(cwd: &Path) -> Option<String> {
    match std::fs::read_to_string(cwd.join("AGENTS.md")) {
        Ok(content) => Some(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::read_to_string(cwd.join("CLAUDE.md")).ok()
        }
        Err(_) => None,
    }
}

/// Load config from `config_dir/rokr.json`, creating it with `"version": 1`
/// if it does not already exist. Never overwrites an existing file; an
/// existing file is parsed and returned as-is.
///
/// Also scaffolds `agents/plan.md` and `agents/build.md` under `config_dir`
/// with default prompt content, if they do not already exist.
pub fn load_or_init(config_dir: &Path) -> Result<Config, ConfigError> {
    std::fs::create_dir_all(config_dir)?;
    let file_path = config_dir.join("rokr.json");

    if file_path.exists() {
        let contents = std::fs::read_to_string(&file_path)?;
        let config: Config =
            serde_json::from_str(&contents).map_err(|source| ConfigError::Invalid {
                path: file_path.clone(),
                source,
            })?;
        if config.version != 1 {
            return Err(ConfigError::UnsupportedVersion {
                path: file_path.clone(),
                found: config.version,
                supported: 1,
            });
        }
        scaffold_agent_prompts(config_dir)?;
        return Ok(config);
    }

    let config = Config {
        version: 1,
        context_window_size: default_context_window_size(),
        auto_compact_threshold: default_auto_compact_threshold(),
    };
    let json = serde_json::to_string_pretty(&config).expect("Config serialization is infallible");
    std::fs::write(&file_path, json)?;
    scaffold_agent_prompts(config_dir)?;
    Ok(config)
}

/// Resolves the rokr config directory: `$XDG_CONFIG_HOME/rokr` if set,
/// otherwise `$HOME/.config/rokr`.
pub fn default_config_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("rokr")
}

/// Load or initialize config at the default, environment-resolved location.
/// Thin wrapper around [`load_or_init`] for use from `main`.
pub fn load_or_init_default() -> Result<Config, ConfigError> {
    load_or_init(&default_config_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_or_init_creates_versioned_config() {
        let temp = tempfile::tempdir().unwrap();

        let config = load_or_init(temp.path()).unwrap();

        assert_eq!(config.version, 1);

        let file_path = temp.path().join("rokr.json");
        let contents = std::fs::read_to_string(&file_path).unwrap();
        assert!(
            contents.contains("\"version\": 1") || contents.contains("\"version\":1"),
            "expected file to contain version 1, got: {contents}"
        );
    }

    #[test]
    fn load_or_init_preserves_existing_config() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("rokr.json");
        let existing = r#"{"version": 1, "custom_user_field": "do-not-touch"}"#;
        std::fs::write(&file_path, existing).unwrap();

        let _ = load_or_init(temp.path()).unwrap();

        let contents = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(
            contents, existing,
            "existing config file must not be modified by load_or_init"
        );
    }

    #[test]
    fn load_or_init_applies_defaults_when_compaction_fields_absent() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("rokr.json");
        let existing = r#"{"version": 1}"#;
        std::fs::write(&file_path, existing).unwrap();

        let config = load_or_init(temp.path()).unwrap();

        let value = serde_json::to_value(&config).unwrap();
        assert_eq!(
            value.get("auto_compact_threshold").and_then(|v| v.as_f64()),
            Some(0.7),
            "expected default auto_compact_threshold of 0.7, got: {value}"
        );
        assert_eq!(
            value.get("context_window_size").and_then(|v| v.as_u64()),
            Some(200_000),
            "expected sane default context_window_size, got: {value}"
        );

        let contents_after = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(
            contents_after, existing,
            "existing config file lacking compaction fields must not be rewritten"
        );
    }

    #[test]
    fn load_or_init_honors_explicit_compaction_settings_when_present() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("rokr.json");
        let existing =
            r#"{"version": 1, "context_window_size": 50000, "auto_compact_threshold": 0.5}"#;
        std::fs::write(&file_path, existing).unwrap();

        let config = load_or_init(temp.path()).unwrap();

        let value = serde_json::to_value(&config).unwrap();
        assert_eq!(
            value.get("context_window_size").and_then(|v| v.as_u64()),
            Some(50000),
            "expected explicit context_window_size to be honored, got: {value}"
        );
        assert_eq!(
            value.get("auto_compact_threshold").and_then(|v| v.as_f64()),
            Some(0.5),
            "expected explicit auto_compact_threshold to be honored, got: {value}"
        );
    }

    #[test]
    fn scaffold_writes_plan_and_build_prompts() {
        let temp = tempfile::tempdir().unwrap();

        let _ = load_or_init(temp.path()).unwrap();

        let plan_path = temp.path().join("agents").join("plan.md");
        let build_path = temp.path().join("agents").join("build.md");

        let plan_contents = std::fs::read_to_string(&plan_path).unwrap_or_else(|e| {
            panic!("expected plan prompt at {plan_path:?} to exist: {e}");
        });
        let build_contents = std::fs::read_to_string(&build_path).unwrap_or_else(|e| {
            panic!("expected build prompt at {build_path:?} to exist: {e}");
        });

        assert!(
            !plan_contents.trim().is_empty(),
            "expected plan.md to have non-empty content"
        );
        assert!(
            !build_contents.trim().is_empty(),
            "expected build.md to have non-empty content"
        );
    }

    #[test]
    fn scaffold_skips_existing_prompt_files() {
        let temp = tempfile::tempdir().unwrap();
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let plan_path = agents_dir.join("plan.md");
        let user_content = "# my custom plan prompt\ndo not touch this";
        std::fs::write(&plan_path, user_content).unwrap();

        let _ = load_or_init(temp.path()).unwrap();

        // Scaffolding still runs for the untouched file, proving this test
        // actually exercises the scaffold path rather than trivially passing
        // because nothing writes to agents/ at all.
        let build_contents =
            std::fs::read_to_string(agents_dir.join("build.md")).unwrap_or_else(|e| {
                panic!("expected build.md to be scaffolded alongside untouched plan.md: {e}")
            });
        assert!(
            !build_contents.trim().is_empty(),
            "expected build.md to have non-empty content"
        );

        let contents = std::fs::read_to_string(&plan_path).unwrap();
        assert_eq!(
            contents, user_content,
            "existing plan.md must not be overwritten by scaffolding"
        );
    }

    #[test]
    fn read_agent_prompt_returns_scaffolded_content() {
        let temp = tempfile::tempdir().unwrap();

        let _ = load_or_init(temp.path()).unwrap();

        let plan_prompt = read_agent_prompt(temp.path(), "plan").unwrap();
        let build_prompt = read_agent_prompt(temp.path(), "build").unwrap();

        assert!(
            plan_prompt.contains("# Plan Agent"),
            "expected plan prompt to contain scaffolded plan.md content, got: {plan_prompt}"
        );
        assert!(
            build_prompt.contains("# Build Agent"),
            "expected build prompt to contain scaffolded build.md content, got: {build_prompt}"
        );
    }

    #[test]
    fn load_or_init_rejects_unsupported_config_version() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("rokr.json");
        let existing = r#"{"version": 2}"#;
        std::fs::write(&file_path, existing).unwrap();

        let result = load_or_init(temp.path());

        assert!(
            matches!(
                result,
                Err(ConfigError::UnsupportedVersion {
                    found: 2,
                    supported: 1,
                    ..
                })
            ),
            "expected UnsupportedVersion{{found: 2, supported: 1}}, got: {result:?}"
        );

        let contents = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(
            contents, existing,
            "config file must not be modified when rejected for unsupported version"
        );
    }

    #[test]
    fn default_config_dir_treats_empty_xdg_config_home_as_unset() {
        static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = ENV_GUARD.lock().unwrap();

        let original_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let original_home = std::env::var_os("HOME");

        std::env::set_var("XDG_CONFIG_HOME", "");
        std::env::set_var("HOME", "/tmp/rokr-test-home");

        let dir = default_config_dir();

        match original_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match original_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(dir, PathBuf::from("/tmp/rokr-test-home/.config/rokr"));
    }

    #[test]
    fn load_project_context_reads_agents_md_when_present() {
        let temp = tempfile::tempdir().unwrap();
        let agents_content = "# Project AGENTS.md\nUse tabs, not spaces, in this repo.";
        std::fs::write(temp.path().join("AGENTS.md"), agents_content).unwrap();

        let context = load_project_context(temp.path());

        assert_eq!(context.as_deref(), Some(agents_content));
    }

    #[test]
    fn load_project_context_falls_back_to_claude_md_when_agents_md_absent() {
        let temp = tempfile::tempdir().unwrap();
        let claude_content = "# Project CLAUDE.md\nRun tests before committing.";
        std::fs::write(temp.path().join("CLAUDE.md"), claude_content).unwrap();

        let context = load_project_context(temp.path());

        assert_eq!(context.as_deref(), Some(claude_content));
    }

    // Unix-only: relies on chmod semantics to make AGENTS.md unreadable,
    // which have no portable Windows equivalent.
    //
    // Note: if this test is ever run as root (e.g. some CI/sandbox setups),
    // permission bits on files owned by root don't block root's own reads,
    // so the read may unexpectedly succeed and this test would need a
    // root-detection guard. No such guard exists elsewhere in this
    // workspace to mirror, so it's intentionally omitted here.
    #[cfg(unix)]
    #[test]
    fn load_project_context_does_not_fall_back_when_agents_md_is_unreadable() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let agents_path = temp.path().join("AGENTS.md");
        let claude_content = "# Project CLAUDE.md\nThis must not be loaded.";
        std::fs::write(&agents_path, "# Project AGENTS.md\nUnreadable.").unwrap();
        std::fs::write(temp.path().join("CLAUDE.md"), claude_content).unwrap();

        std::fs::set_permissions(&agents_path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let context = load_project_context(temp.path());

        // Restore permissions before the tempdir drops, so cleanup can
        // delete the file.
        std::fs::set_permissions(&agents_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert_ne!(
            context.as_deref(),
            Some(claude_content),
            "must not silently fall back to CLAUDE.md when AGENTS.md exists but is unreadable"
        );
        assert_eq!(
            context, None,
            "a non-NotFound read error on AGENTS.md must yield no project context"
        );
    }
}
