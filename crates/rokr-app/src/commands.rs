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
#[derive(Clone)]
pub struct CustomCommand {
    pub template: String,
    /// Frontmatter `description:` value, if present.
    pub description: Option<String>,
    /// Frontmatter `argument-hint:` value, if present.
    pub argument_hint: Option<String>,
    /// Frontmatter `agent:` value, if present. F-017 (pre-ship review):
    /// parsed and stored for future use, but NOT YET honored anywhere --
    /// there is no agent-dispatch logic for custom commands yet. This is
    /// deliberately the cheapest compliant fix (parse + carry the field),
    /// not an implementation of agent dispatch.
    pub agent: Option<String>,
}

/// Discovers and holds user-scope custom commands (markdown files under
/// `config_dir/commands/`), keyed by filename stem.
#[derive(Clone)]
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
        Some(self.resolve_skills(&expanded))
    }

    /// F-007 (PRD story 10, pre-ship review): resolves `@skill:<name>`
    /// mentions in ANY text, not just inside a command template's expanded
    /// body -- [`Self::expand`] above already applies this to a template's
    /// expansion, but that only runs for `/`-prefixed input naming a
    /// discovered command. Exposed publicly so callers can apply the same
    /// mention resolution to a plain prompt the user typed directly (no
    /// leading `/`, never touches `expand` at all) -- see
    /// [`resolve_skill_mentions`]'s doc comment for the mention
    /// syntax/lookup contract this delegates to.
    pub fn resolve_skills(&self, text: &str) -> String {
        resolve_skill_mentions(text, &self.skills)
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
/// YAML parser -- deliberately, since the ticket only requires a handful of
/// known flat string keys (`description`, `argument-hint`, `agent` -- F-017,
/// pre-ship review) and no YAML-parsing crate is already a dependency
/// anywhere in this workspace (checked before choosing this over adding
/// one; see the ticket report's Assumptions/deviations section). Any other
/// key is silently ignored; a key with no `:` is skipped rather than
/// treated as an error, matching this parser's overall tolerant-absence
/// contract (see `scan_commands_dir`'s doc comment).
fn parse_command_file(contents: &str) -> CustomCommand {
    let Some(rest) = contents.strip_prefix("---\n") else {
        return CustomCommand {
            template: contents.to_string(),
            description: None,
            argument_hint: None,
            agent: None,
        };
    };
    let Some(end) = rest.find("\n---") else {
        return CustomCommand {
            template: contents.to_string(),
            description: None,
            argument_hint: None,
            agent: None,
        };
    };
    let frontmatter = &rest[..end];
    let after = &rest[end + "\n---".len()..];
    let body = after.strip_prefix('\n').unwrap_or(after);

    let mut description = None;
    let mut argument_hint = None;
    let mut agent = None;
    for line in frontmatter.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().to_string();
        match key.trim() {
            "description" => description = Some(value),
            "argument-hint" => argument_hint = Some(value),
            "agent" => agent = Some(value),
            _ => {}
        }
    }

    CustomCommand {
        template: body.to_string(),
        description,
        argument_hint,
        agent,
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

    /// F-017 (pre-ship review): `description`, `argument-hint`, and `agent`
    /// frontmatter fields must all be parsed and carried on the resulting
    /// `CustomCommand`, not silently dropped. `agent` is parsed here but not
    /// yet honored anywhere (no agent-dispatch logic exists for custom
    /// commands yet -- see `CustomCommand::agent`'s doc comment); this test
    /// only locks in that the value survives parsing.
    #[test]
    fn command_file_frontmatter_description_argument_hint_and_agent_are_parsed() {
        let temp = tempfile::tempdir().unwrap();
        let commands_dir = temp.path().join("commands");
        fs::create_dir_all(&commands_dir).unwrap();
        fs::write(
            commands_dir.join("release.md"),
            "---\ndescription: Cut a release\nargument-hint: <version>\nagent: release-bot\n---\nRelease $1",
        )
        .unwrap();

        let registry = CommandRegistry::discover_user_scope(temp.path());
        let command = registry
            .get("release")
            .expect("expected 'release' to be discovered from commands/");

        assert_eq!(
            command.description.as_deref(),
            Some("Cut a release"),
            "expected the 'description' frontmatter field to be parsed"
        );
        assert_eq!(
            command.argument_hint.as_deref(),
            Some("<version>"),
            "expected the 'argument-hint' frontmatter field to be parsed"
        );
        assert_eq!(
            command.agent.as_deref(),
            Some("release-bot"),
            "expected the 'agent' frontmatter field to be parsed (not yet honored elsewhere)"
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
