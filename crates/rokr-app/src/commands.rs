//! Ticket 63 (custom-command-discovery-and-registry): user-scope discovery
//! of markdown-templated `/command` files, keyed by filename stem, plus
//! `$ARGUMENTS`/`$1`../`$n` template expansion. See `CommandRegistry`'s doc
//! comment for the discovery/expansion contract. Ticket 64
//! (custom-command-project-scope-and-trust-boundary) added project-scope
//! discovery (`discover_project_scope`) and merge precedence
//! (`merge_overriding`, project scope over user scope) -- see ADR 0014 for
//! the trust-boundary reasoning behind allowing project-scope discovery at
//! all. Ticket 65 (skills-instruction-bundle-loading) added `@skill:<name>`
//! mention resolution: a loadable-instruction-bundle markdown file under a
//! `skills/` directory sibling to `commands/` at each scope, discovered the
//! same way commands are and merged with the same project-wins-on-collision
//! precedence, gets its full contents inlined in place of the mention
//! during template expansion. See `resolve_skill_mentions`'s doc comment.

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
    /// Discovered skill markdown files, keyed by filename stem (skill name),
    /// mapped to the file's raw, full contents. Unlike `CustomCommand`,
    /// skills have no frontmatter/metadata concept -- the mapped value is
    /// exactly what was read off disk, unparsed.
    skills: HashMap<String, String>,
}

impl CommandRegistry {
    /// Scans `config_dir/commands/*.md`, keying each by filename stem
    /// (filename minus the `.md` extension). Missing/unreadable directory,
    /// or an unreadable individual file, is not an error -- it's simply
    /// absent from the registry (a fresh user has no `commands/` dir yet).
    pub fn discover_user_scope(config_dir: &Path) -> Self {
        CommandRegistry {
            commands: scan_commands_dir(&config_dir.join("commands")),
            skills: scan_skills_dir(&config_dir.join("skills")),
        }
    }

    /// Scans `project_dir/.rokr/commands/*.md` -- same discovery/parsing
    /// contract as [`Self::discover_user_scope`], just rooted at a
    /// project-local directory instead of the user's config dir. See ADR
    /// 0014 for why project-scope discovery of these text-only templates is
    /// allowed at all (unlike hooks/MCP's user-scope-only boundary, ADR
    /// 0012).
    pub fn discover_project_scope(project_dir: &Path) -> Self {
        CommandRegistry {
            commands: scan_commands_dir(&project_dir.join(".rokr").join("commands")),
            skills: scan_skills_dir(&project_dir.join(".rokr").join("skills")),
        }
    }

    /// Folds `other`'s commands and skills into `self`, with `other`'s
    /// entries winning on a same-name collision. Callers merge project scope
    /// OVER user scope (`user.merge_overriding(project)`) so a project's own
    /// command or skill shadows a same-named personal one -- see ADR 0014,
    /// decision 3.
    pub fn merge_overriding(&mut self, other: CommandRegistry) {
        self.commands.extend(other.commands);
        self.skills.extend(other.skills);
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
        let expanded = expand_template(&command.template, args);
        Some(resolve_skill_mentions(&expanded, &self.skills))
    }
}

/// Scans `dir` for `*.md` files, keying each by filename stem (filename
/// minus the `.md` extension) and parsing its contents via
/// [`parse_command_file`]. Shared by [`CommandRegistry::discover_user_scope`]
/// and [`CommandRegistry::discover_project_scope`] -- the only difference
/// between the two is which directory gets passed in. Missing/unreadable
/// `dir`, or an unreadable individual file, is not an error -- it's simply
/// absent from the result (a fresh user/project has no `commands/` dir
/// yet).
fn scan_commands_dir(dir: &Path) -> HashMap<String, CustomCommand> {
    let mut commands = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
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
    commands
}

/// Scans `dir` for `*.md` files, keying each by filename stem (filename
/// minus the `.md` extension) and storing its raw, full contents directly --
/// unlike [`scan_commands_dir`], no [`parse_command_file`] call, since
/// skills have no frontmatter/metadata concept. Same tolerant-absence
/// contract as `scan_commands_dir`: missing/unreadable `dir`, or an
/// unreadable individual file, is not an error -- it's simply absent from
/// the result.
fn scan_skills_dir(dir: &Path) -> HashMap<String, String> {
    let mut skills = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
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
            skills.insert(stem.to_string(), contents);
        }
    }
    skills
}

/// Resolves `@skill:<name>` mentions in `text` by inlining the full
/// contents of the named entry in `skills` in place of the mention.
/// `<name>` is read as a run of `[A-Za-z0-9_-]` characters immediately
/// following `@skill:`, stopping at the first character outside that set
/// (whitespace, punctuation, or end of string) -- same "bare token" shape
/// filename stems (and thus skill/command names) already have. A mention
/// naming a skill NOT present in `skills` is left as literal, unexpanded
/// text in the output -- there's no error-reporting channel available at
/// expansion time, mirroring how `expand_template` already leaves an
/// unrecognized `$ARGUMENTS`/`$1`-style placeholder as literal text too.
fn resolve_skill_mentions(text: &str, skills: &HashMap<String, String>) -> String {
    const MARKER: &str = "@skill:";
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(marker_index) = rest.find(MARKER) {
        result.push_str(&rest[..marker_index]);
        let after_marker = &rest[marker_index + MARKER.len()..];
        let name_len = after_marker
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(after_marker.len());
        let name = &after_marker[..name_len];
        match skills.get(name) {
            Some(contents) => result.push_str(contents),
            None => {
                result.push_str(MARKER);
                result.push_str(name);
            }
        }
        rest = &after_marker[name_len..];
    }
    result.push_str(rest);
    result
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
    fn project_scope_command_body_expands_to_inert_text_never_executed() {
        let temp = tempfile::tempdir().unwrap();
        let commands_dir = temp.path().join(".rokr").join("commands");
        fs::create_dir_all(&commands_dir).unwrap();
        fs::write(
            commands_dir.join("deploy.md"),
            "---\ndescription: test\n---\nRun: !rm -rf /tmp/should-never-execute && echo pwned",
        )
        .unwrap();

        let registry = CommandRegistry::discover_project_scope(temp.path());

        let expanded = registry
            .expand("/deploy")
            .expect("expected /deploy to be discovered from .rokr/commands/");

        assert_eq!(
            expanded,
            "Run: !rm -rf /tmp/should-never-execute && echo pwned",
            "expected the '!'-prefixed body to expand to inert literal text, byte-for-byte \
             unchanged -- CommandRegistry has no shell-execution semantics (see ADR 0014)"
        );
    }

    #[test]
    fn built_in_command_name_always_wins_over_a_same_named_discovered_command() {
        // "cost" is a real built-in (see crates/rokr/src/main.rs's `command`
        // closure, match arm "/cost"). CommandRegistry has and needs NO concept
        // of built-in names -- it happily discovers a same-named command from
        // either scope. The "built-in always wins" guarantee lives entirely at
        // the CALL SITE (rokr_tui::run's resolve_custom_command is only ever
        // consulted from the built-in dispatcher's own "unknown command"
        // fallthrough -- see that function's doc comment), not inside this
        // registry. This test locks in both halves: the registry doesn't
        // filter the collision away, and replicating the real call order still
        // makes the built-in win regardless of what's merged into the registry.
        let temp = tempfile::tempdir().unwrap();
        let user_commands_dir = temp.path().join("commands");
        let project_dir = temp.path().join("project");
        let project_commands_dir = project_dir.join(".rokr").join("commands");
        fs::create_dir_all(&user_commands_dir).unwrap();
        fs::create_dir_all(&project_commands_dir).unwrap();
        fs::write(user_commands_dir.join("cost.md"), "user-scope /cost template").unwrap();
        fs::write(project_commands_dir.join("cost.md"), "project-scope /cost template").unwrap();

        let mut registry = CommandRegistry::discover_user_scope(temp.path());
        registry.merge_overriding(CommandRegistry::discover_project_scope(&project_dir));

        assert!(
            registry.get("cost").is_some(),
            "expected the registry to hold a 'cost' entry -- it has no built-in awareness to \
             filter it out"
        );

        // Mirrors main.rs's real dispatch shape: the built-in match runs to
        // completion FIRST; the registry is only ever consulted from that
        // match's fallthrough arm.
        fn dispatch(input: &str, registry: &CommandRegistry) -> String {
            match input {
                "/cost" => "built-in cost output".to_string(),
                _ => registry
                    .expand(input)
                    .unwrap_or_else(|| format!("unknown command: {input}")),
            }
        }

        assert_eq!(
            dispatch("/cost", &registry),
            "built-in cost output",
            "expected the built-in '/cost' handler to win over the discovered command of the \
             same name"
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

    /// Ticket 65 (skills-instruction-bundle-loading): `@skill:<name>` inside
    /// a command template is a loadable-instruction-bundle mention -- it
    /// expands to the full contents of a same-named markdown file under a
    /// `skills/` directory sibling to `commands/`, discovered the same way
    /// `CommandRegistry` discovers commands themselves.
    #[test]
    fn skill_mention_resolves_to_named_skill_file_contents_from_scoped_directory() {
        let temp = tempfile::tempdir().unwrap();
        let commands_dir = temp.path().join("commands");
        let skills_dir = temp.path().join("skills");
        fs::create_dir_all(&commands_dir).unwrap();
        fs::create_dir_all(&skills_dir).unwrap();
        fs::write(
            commands_dir.join("review.md"),
            "---\ndescription: test\n---\nFollow @skill:code-style",
        )
        .unwrap();
        fs::write(
            skills_dir.join("code-style.md"),
            "# Code Style\nUse 4-space indentation.",
        )
        .unwrap();

        let registry = CommandRegistry::discover_user_scope(temp.path());

        let expanded = registry
            .expand("/review")
            .expect("expected /review to be discovered from commands/");

        assert_eq!(
            expanded,
            "Follow # Code Style\nUse 4-space indentation.",
            "expected @skill:code-style to be replaced inline with the skill file's full \
             contents, sourced from the user-scope skills/ directory"
        );
    }

    /// Ticket 65 (skills-instruction-bundle-loading): when a user-scope and
    /// a project-scope skill share the same name, the project-scope one
    /// must win -- same precedence `CommandRegistry` already gives
    /// project-scope commands over user-scope commands (ADR 0014, decision
    /// 3), extended here to skills.
    #[test]
    fn project_scope_skill_wins_over_user_scope_skill_of_the_same_name() {
        let temp = tempfile::tempdir().unwrap();
        let user_commands_dir = temp.path().join("commands");
        let user_skills_dir = temp.path().join("skills");
        let project_dir = temp.path().join("project");
        let project_skills_dir = project_dir.join(".rokr").join("skills");
        fs::create_dir_all(&user_commands_dir).unwrap();
        fs::create_dir_all(&user_skills_dir).unwrap();
        fs::create_dir_all(&project_skills_dir).unwrap();
        fs::write(
            user_commands_dir.join("review.md"),
            "---\ndescription: test\n---\nFollow @skill:code-style",
        )
        .unwrap();
        fs::write(user_skills_dir.join("code-style.md"), "USER-SCOPE-STYLE-GUIDE").unwrap();
        fs::write(
            project_skills_dir.join("code-style.md"),
            "PROJECT-SCOPE-STYLE-GUIDE",
        )
        .unwrap();

        let mut registry = CommandRegistry::discover_user_scope(temp.path());
        registry.merge_overriding(CommandRegistry::discover_project_scope(&project_dir));

        let expanded = registry
            .expand("/review")
            .expect("expected /review to be discovered from user-scope commands/");

        assert_eq!(
            expanded,
            "Follow PROJECT-SCOPE-STYLE-GUIDE",
            "expected the project-scope skill to win over the user-scope skill of the same name"
        );
    }
}
