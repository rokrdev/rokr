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
}

/// Load config from `config_dir/rokr.json`, creating it with `"version": 1`
/// if it does not already exist. Never overwrites an existing file; an
/// existing file is parsed and returned as-is.
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
        return Ok(config);
    }

    let config = Config { version: 1 };
    let json = serde_json::to_string_pretty(&config).expect("Config serialization is infallible");
    std::fs::write(&file_path, json)?;
    Ok(config)
}

/// Resolves the rokr config directory: `$XDG_CONFIG_HOME/rokr` if set,
/// otherwise `$HOME/.config/rokr`.
pub fn default_config_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
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
}
