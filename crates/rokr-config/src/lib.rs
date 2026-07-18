//! JSON configuration loading, schema versioning, and migrations.

use std::path::{Path, PathBuf};

/// The on-disk config schema. See docs/adr/0002-config-format-and-versioning.md.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Config {
    pub version: u32,
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

    let config = Config { version: 1 };
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
}
