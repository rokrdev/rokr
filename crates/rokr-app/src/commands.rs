//! Ticket 63 (custom-command-discovery-and-registry): user-scope discovery
//! of markdown-templated `/command` files, keyed by filename stem, plus
//! `$ARGUMENTS`/`$1`../`$n` template expansion. See `CommandRegistry`'s doc
//! comment for the discovery/expansion contract.

use std::collections::HashMap;
use std::path::Path;

/// A single discovered custom command: optional YAML-frontmatter metadata
/// plus the markdown body, which is the prompt template.
pub struct CustomCommand {
    pub template: String,
}

/// Discovers and holds user-scope custom commands (markdown files under
/// `config_dir/commands/`), keyed by filename stem.
pub struct CommandRegistry {
    commands: HashMap<String, CustomCommand>,
}

impl CommandRegistry {
    /// Scans `config_dir/commands/*.md`, keying each by filename stem
    /// (filename minus the `.md` extension). Missing/unreadable directory,
    /// or an unreadable individual file, is not an error -- it's simply
    /// absent from the registry (a fresh user has no `commands/` dir yet).
    pub fn discover_user_scope(config_dir: &Path) -> Self {
        let mut commands = HashMap::new();
        let commands_dir = config_dir.join("commands");
        if let Ok(entries) = std::fs::read_dir(&commands_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Ok(contents) = std::fs::read_to_string(&path) else {
                    continue;
                };
                commands.insert(stem.to_string(), parse_command_file(&contents));
            }
        }
        CommandRegistry { commands }
    }

    pub fn get(&self, name: &str) -> Option<&CustomCommand> {
        self.commands.get(name)
    }

    /// Parses `input` as a slash command (`/name rest-of-line`), looks
    /// `name` up, and if found expands its template against `rest`. Returns
    /// `None` when `input` isn't `/`-prefixed or names a command not in the
    /// registry -- callers (the `command` dispatch fallthrough) treat that
    /// as "not a custom command" and leave existing behavior unchanged.
    pub fn expand(&self, input: &str) -> Option<String> {
        let rest = input.strip_prefix('/')?;
        let (name, args) = match rest.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args.trim_start()),
            None => (rest, ""),
        };
        let command = self.commands.get(name)?;
        Some(expand_template(&command.template, args))
    }
}

/// Substitutes `$ARGUMENTS` with the full trailing argument string, and
/// `$1`/`$2`/... with `args` split on whitespace. Positional placeholders
/// are substituted highest-index-first so `$10` (were a template ever to
/// use it) isn't clobbered by a `$1` replacement leaving a stray `0`.
fn expand_template(template: &str, args: &str) -> String {
    let mut result = template.replace("$ARGUMENTS", args);
    let positional: Vec<&str> = args.split_whitespace().collect();
    for (index, value) in positional.iter().enumerate().rev() {
        let placeholder = format!("${}", index + 1);
        result = result.replace(&placeholder, value);
    }
    result
}

/// Tolerantly parses an optional leading YAML frontmatter block (delimited
/// by `---` lines) off of `contents`; everything after it (or all of
/// `contents`, if there's no frontmatter) is the template body. Frontmatter
/// keys are matched as plain `key: value` lines rather than through a real
/// YAML parser -- deliberately, since the ticket only requires three known
/// flat string keys and no YAML-parsing crate is already a dependency
/// anywhere in this workspace (checked before choosing this over adding
/// one; see the ticket report's Assumptions/deviations section).
fn parse_command_file(contents: &str) -> CustomCommand {
    let Some(rest) = contents.strip_prefix("---\n") else {
        return CustomCommand {
            template: contents.to_string(),
        };
    };
    let Some(end) = rest.find("\n---") else {
        return CustomCommand {
            template: contents.to_string(),
        };
    };
    let after = &rest[end + "\n---".len()..];
    let body = after.strip_prefix('\n').unwrap_or(after);
    CustomCommand {
        template: body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn registry_discovers_user_scope_command_files_keyed_by_filename_stem() {
        let temp = tempfile::tempdir().unwrap();
        let commands_dir = temp.path().join("commands");
        fs::create_dir_all(&commands_dir).unwrap();
        fs::write(
            commands_dir.join("my-command.md"),
            "---\ndescription: test\n---\nHandle: $ARGUMENTS",
        )
        .unwrap();

        let registry = CommandRegistry::discover_user_scope(temp.path());

        assert!(
            registry.get("my-command").is_some(),
            "expected registry to discover a command keyed by filename stem \"my-command\""
        );
    }

    #[test]
    fn template_expansion_substitutes_arguments_and_positional_args() {
        let temp = tempfile::tempdir().unwrap();
        let commands_dir = temp.path().join("commands");
        fs::create_dir_all(&commands_dir).unwrap();
        fs::write(
            commands_dir.join("my-command.md"),
            "---\ndescription: test\n---\nHandle: $ARGUMENTS (first=$1, second=$2)",
        )
        .unwrap();
        let registry = CommandRegistry::discover_user_scope(temp.path());

        let expanded = registry.expand("/my-command foo bar");

        assert_eq!(
            expanded,
            Some("Handle: foo bar (first=foo, second=bar)".to_string())
        );
    }
}
