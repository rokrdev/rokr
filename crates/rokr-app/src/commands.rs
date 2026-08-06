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
use std::path::{Path, PathBuf};

use rokr_tools::Tool;

use crate::skill_trust::{ConsentOutcome, ConsentResolver, SkillScope, SkillTrustStore};

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

/// A single discovered skill file (ticket 65: skills-instruction-bundle-loading;
/// ticket 75: executable-skill-invocation, ADR 0018). `contents` is always
/// the file's raw, full, untouched text -- what gets inlined verbatim for an
/// INERT skill mention (ticket 65's unchanged behavior), regardless of
/// whether the file happens to carry frontmatter at all. `run`, when
/// present, is the literal value of an optional `run:` frontmatter field
/// (no interpolation ever applied to it -- ADR 0018 decision 6); its
/// presence, not any change to what gets inlined for an inert skill, is what
/// distinguishes an executable skill from an inert one. `scope` records
/// which directory this skill was discovered under -- ADR 0018 decision 5:
/// only a `Project`-scope executable skill is ever TOFU-gated; `User`-scope
/// is auto-trusted.
#[derive(Clone)]
struct Skill {
    path: PathBuf,
    contents: String,
    run: Option<String>,
    scope: SkillScope,
}

/// Discovers and holds user-scope custom commands (markdown files under
/// `config_dir/commands/`), keyed by filename stem.
#[derive(Clone)]
pub struct CommandRegistry {
    commands: HashMap<String, CustomCommand>,
    /// Discovered skill markdown files, keyed by filename stem (skill name).
    /// See [`Skill`]'s doc comment.
    skills: HashMap<String, Skill>,
}

impl CommandRegistry {
    /// Scans `config_dir/commands/*.md`, keying each by filename stem
    /// (filename minus the `.md` extension). Missing/unreadable directory,
    /// or an unreadable individual file, is not an error -- it's simply
    /// absent from the registry (a fresh user has no `commands/` dir yet).
    pub fn discover_user_scope(config_dir: &Path) -> Self {
        CommandRegistry {
            commands: scan_commands_dir(&config_dir.join("commands")),
            skills: scan_skills_dir(&config_dir.join("skills"), SkillScope::User),
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
            skills: scan_skills_dir(
                &project_dir.join(".rokr").join("skills"),
                SkillScope::Project,
            ),
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
        // Ticket 75 (executable-skill-invocation), ADR 0018: `expand`
        // deliberately does NOT call the full, consent-aware
        // `Self::resolve_skills` below -- the ADR names only `main.rs`'s
        // `submit` closure and `headless.rs`'s prompt-resolution path as
        // `resolve_skills`'s two call sites, not this one. An INERT
        // `@skill:` mention embedded in a command template is still
        // resolved here, exactly as ticket 65 always did (see
        // `inline_inert_skill_mentions`'s doc comment); an EXECUTABLE one is
        // deliberately left as literal text so it survives, unresolved,
        // into the expanded prompt -- the real call site downstream (both
        // of which call `resolve_skills` unconditionally on every submitted
        // prompt, per ticket 65's F-007 fix) gets the real chance to gate it
        // with full consent machinery.
        Some(inline_inert_skill_mentions(&expanded, &self.skills))
    }

    /// F-007 (PRD story 10, pre-ship review): resolves `@skill:<name>`
    /// mentions in ANY text, not just inside a command template's expanded
    /// body -- [`Self::expand`] above already applies
    /// [`inline_inert_skill_mentions`] to a template's expansion, but that
    /// only runs for `/`-prefixed input naming a discovered command, and
    /// only ever resolves INERT skill mentions (see that function's doc
    /// comment). This method is the full, ADR-0018-aware resolution: an
    /// inert mention behaves identically; an EXECUTABLE mention (a skill
    /// with a `run:` frontmatter command) is gated by consent (decision 1),
    /// then executed through the sandboxed `rokr_tools::bash::BashTool` path
    /// (decision 2, unchanged since ticket 69) with its captured stdout
    /// inlined in place of the mention.
    ///
    /// `&self` is only borrowed for the duration of this call -- every field
    /// this needs is cloned out eagerly before the returned future is built,
    /// so the future itself borrows nothing and is `Send + 'static`.
    /// Mirrors `SessionRunner::run_submission`'s own established shape
    /// (`crate::runner`), which exists for the identical reason: a caller
    /// (`main.rs`'s `submit` closure) invoked repeatedly needs to construct
    /// a fresh, owned, awaitable future from a borrowed `&self` each time.
    pub fn resolve_skills<C: ConsentResolver + 'static>(
        &self,
        text: &str,
        workspace_root: PathBuf,
        trust_store: SkillTrustStore,
        consent: C,
    ) -> impl std::future::Future<Output = Result<String, String>> + Send + 'static {
        let text = text.to_string();
        let skills = self.skills.clone();
        async move {
            resolve_skill_mentions_with_consent(
                &text,
                &skills,
                workspace_root,
                trust_store,
                consent,
            )
            .await
        }
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
/// minus the `.md` extension). `contents` is always stored raw/untouched
/// (see [`Skill`]'s doc comment); [`parse_skill_run_field`] additionally,
/// tolerantly extracts an optional `run:` frontmatter value (ticket 75, ADR
/// 0018) without altering what gets stored as `contents`. Every discovered
/// entry is tagged with `scope` (ADR 0018 decision 5). Same tolerant-absence
/// contract as `scan_commands_dir`: missing/unreadable `dir`, or an
/// unreadable individual file, is not an error -- it's simply absent from
/// the result.
fn scan_skills_dir(dir: &Path, scope: SkillScope) -> HashMap<String, Skill> {
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
            let run = parse_skill_run_field(&contents);
            skills.insert(
                stem.to_string(),
                Skill {
                    path,
                    contents,
                    run,
                    scope,
                },
            );
        }
    }
    skills
}

/// Reads a `@skill:<name>` mention immediately following `MARKER` at the
/// start of `after_marker`, returning `(name, remaining_text_after_the_name)`.
/// `<name>` is a run of `[A-Za-z0-9_-]` characters, stopping at the first
/// character outside that set (whitespace, punctuation, or end of string) --
/// same "bare token" shape filename stems (and thus skill/command names)
/// already have. Shared by [`inline_inert_skill_mentions`] and
/// [`resolve_skill_mentions_with_consent`] so both scanners agree on the
/// exact same mention-name boundary rule.
fn read_skill_mention_name(after_marker: &str) -> (&str, &str) {
    let name_len = after_marker
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(after_marker.len());
    (&after_marker[..name_len], &after_marker[name_len..])
}

const SKILL_MENTION_MARKER: &str = "@skill:";

/// Resolves ONLY inert (no `run:` frontmatter) `@skill:<name>` mentions in
/// `text` by inlining the full, raw contents of the named entry in `skills`
/// in place of the mention -- ticket 65's original, unchanged behavior. A
/// mention naming a skill NOT present in `skills` is left as literal,
/// unexpanded text in the output -- there's no error-reporting channel
/// available at expansion time, mirroring how `expand_template` already
/// leaves an unrecognized `$ARGUMENTS`/`$1`-style placeholder as literal
/// text too. Ticket 75 (ADR 0018) addition: a mention naming an EXECUTABLE
/// skill (`run.is_some()`) is ALSO left as literal, unexpanded text here --
/// deliberately, not a bug -- so it survives, unresolved, through to the
/// real, consent-aware [`CommandRegistry::resolve_skills`] call downstream
/// at its two ADR-designated call sites. This function is used only by
/// [`CommandRegistry::expand`]; see that method's doc comment for why it
/// must not itself gain consent-checking behavior.
fn inline_inert_skill_mentions(text: &str, skills: &HashMap<String, Skill>) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(marker_index) = rest.find(SKILL_MENTION_MARKER) {
        result.push_str(&rest[..marker_index]);
        let after_marker = &rest[marker_index + SKILL_MENTION_MARKER.len()..];
        let (name, remaining) = read_skill_mention_name(after_marker);
        match skills.get(name) {
            Some(skill) if skill.run.is_none() => result.push_str(&skill.contents),
            _ => {
                // Not found, OR found but executable -- left as literal
                // text; see this function's doc comment.
                result.push_str(SKILL_MENTION_MARKER);
                result.push_str(name);
            }
        }
        rest = remaining;
    }
    result.push_str(rest);
    result
}

/// The full, ADR-0018-aware `@skill:<name>` mention resolver: an inert
/// mention behaves exactly like [`inline_inert_skill_mentions`]; an
/// executable one (`run.is_some()`) is gated per ADR 0018 decisions 1 and 5
/// before ever executing, then run through the sandboxed
/// `rokr_tools::bash::BashTool` path (decision 2, unchanged since ticket
/// 69) -- built ONCE per call, reused for every executable mention found.
/// Never applies any interpolation to the `run:` value (decision 6): the
/// exact string shown in the consent prompt is byte-identical to the exact
/// string that executes.
async fn resolve_skill_mentions_with_consent<C: ConsentResolver>(
    text: &str,
    skills: &HashMap<String, Skill>,
    workspace_root: PathBuf,
    trust_store: SkillTrustStore,
    consent: C,
) -> Result<String, String> {
    let bash = rokr_tools::bash::BashTool::new(workspace_root);
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(marker_index) = rest.find(SKILL_MENTION_MARKER) {
        result.push_str(&rest[..marker_index]);
        let after_marker = &rest[marker_index + SKILL_MENTION_MARKER.len()..];
        let (name, remaining) = read_skill_mention_name(after_marker);
        rest = remaining;

        let Some(skill) = skills.get(name) else {
            result.push_str(SKILL_MENTION_MARKER);
            result.push_str(name);
            continue;
        };
        let Some(command) = &skill.run else {
            result.push_str(&skill.contents);
            continue;
        };

        // ADR 0018 decision 5: `User`-scope executes unconditionally --
        // scope alone (not a stored grant) is what exempts it, so the
        // `ConsentResolver` is never even constructed a request for.
        // `Project`-scope is TOFU hash-pinned: an already-trusted
        // `(path, hash)` pair also skips the resolver (that's the whole
        // point of TOFU -- the prompt only happens once per version), an
        // untrusted one consults it.
        let should_execute = match skill.scope {
            SkillScope::User => true,
            SkillScope::Project => {
                let hash = crate::skill_trust::hash_skill_contents(&skill.contents);
                if trust_store.is_trusted(&skill.path, &hash) {
                    true
                } else {
                    let request = crate::skill_trust::SkillConsentRequest {
                        command: command.clone(),
                        skill_path: skill.path.clone(),
                        scope: skill.scope,
                        name: name.to_string(),
                        hash: hash.clone(),
                    };
                    match consent.resolve(request).await {
                        ConsentOutcome::ApproveAndPersist => {
                            trust_store
                                .grant(&skill.path, &hash)
                                .map_err(|err| err.to_string())?;
                            true
                        }
                        ConsentOutcome::ApproveWithoutPersisting => true,
                        ConsentOutcome::Decline => false,
                    }
                }
            }
        };

        if !should_execute {
            // ADR 0018 decision 1 (ruling 3): NEVER the skill's body here --
            // it may assume its command already ran.
            result.push_str(&format!("[skill '{name}' not executed]"));
            continue;
        }

        match bash
            .execute(serde_json::json!({ "command": command }))
            .await
        {
            Ok(stdout) => result.push_str(stdout.trim_end_matches('\n')),
            Err(err) => result.push_str(&format!("[skill '{name}' run command failed: {err}]")),
        }
    }
    result.push_str(rest);
    Ok(result)
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

/// Tolerantly splits a leading `---`-delimited frontmatter block off of
/// `contents`, returning `(frontmatter_text, body)` -- `None` when `contents`
/// doesn't open with a `---\n` line, or has no closing `\n---` line. Shared
/// by [`parse_command_file`] and, ticket 75 (ADR 0018), [`parse_skill_run_field`]
/// so both agree on the exact same delimiter rule.
fn split_frontmatter(contents: &str) -> Option<(&str, &str)> {
    let rest = contents.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let frontmatter = &rest[..end];
    let after = &rest[end + "\n---".len()..];
    let body = after.strip_prefix('\n').unwrap_or(after);
    Some((frontmatter, body))
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
    let Some((frontmatter, body)) = split_frontmatter(contents) else {
        return CustomCommand {
            template: contents.to_string(),
            description: None,
            argument_hint: None,
            agent: None,
        };
    };

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

fn parse_skill_run_field(contents: &str) -> Option<String> {
    let (frontmatter, _body) = split_frontmatter(contents)?;
    for line in frontmatter.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim() == "run" {
            let value = value.trim();
            // M-3 (pre-ship review): an empty or whitespace-only `run:`
            // value must not be treated as an executable skill at all --
            // without this guard, `Some("")` reaches `resolve_skills`,
            // which gates it behind a real consent prompt only to run
            // `sh -c ""` (a no-op) on approval, a pointless prompt for a
            // command that does nothing.
            return if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            };
        }
    }
    None
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
            expanded, "Run: !rm -rf /tmp/should-never-execute && echo pwned",
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
        fs::write(
            user_commands_dir.join("cost.md"),
            "user-scope /cost template",
        )
        .unwrap();
        fs::write(
            project_commands_dir.join("cost.md"),
            "project-scope /cost template",
        )
        .unwrap();

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
            expanded, "Follow # Code Style\nUse 4-space indentation.",
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
        fs::write(
            user_skills_dir.join("code-style.md"),
            "USER-SCOPE-STYLE-GUIDE",
        )
        .unwrap();
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
            expanded, "Follow PROJECT-SCOPE-STYLE-GUIDE",
            "expected the project-scope skill to win over the user-scope skill of the same name"
        );
    }

    // ---------------------------------------------------------------
    // Ticket 75 (executable-skill-invocation), ADR 0018.
    // ---------------------------------------------------------------

    /// A test `ConsentResolver` that records whether it was ever consulted
    /// and always declines. Shared `Arc<AtomicBool>` so clones (a fresh
    /// clone is handed to each `resolve_skills` call below) report into the
    /// same flag.
    #[derive(Clone, Default)]
    struct RecordingConsentResolver {
        asked: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl RecordingConsentResolver {
        fn was_asked(&self) -> bool {
            self.asked.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl crate::skill_trust::ConsentResolver for RecordingConsentResolver {
        fn resolve(
            &self,
            _request: crate::skill_trust::SkillConsentRequest,
        ) -> impl std::future::Future<Output = crate::skill_trust::ConsentOutcome> + Send {
            self.asked.store(true, std::sync::atomic::Ordering::SeqCst);
            async { crate::skill_trust::ConsentOutcome::Decline }
        }
    }

    /// A test `ConsentResolver` that panics if ever consulted -- proves a
    /// user-scope executable skill's auto-trust comes from SCOPE ALONE, not
    /// from ever asking (and getting lucky with an auto-approve stub).
    #[derive(Clone)]
    struct PanicsIfConsultedConsentResolver;

    impl crate::skill_trust::ConsentResolver for PanicsIfConsultedConsentResolver {
        fn resolve(
            &self,
            _request: crate::skill_trust::SkillConsentRequest,
        ) -> impl std::future::Future<Output = crate::skill_trust::ConsentOutcome> + Send {
            async {
                panic!(
                    "ConsentResolver must never be consulted for a user-scope executable skill \
                     -- scope alone (not a stored grant) is what exempts it"
                )
            }
        }
    }

    fn fresh_trust_store(temp: &std::path::Path) -> crate::skill_trust::SkillTrustStore {
        crate::skill_trust::SkillTrustStore::new(&temp.join("config"))
    }

    /// A skill file with `run:` in its frontmatter must be recognized as
    /// executable (triggers a consent check) -- distinct from an inert
    /// skill (no `run:` field at all), which must keep ticket 65's exact
    /// inert-inlining behavior unchanged and never consult consent at all.
    #[tokio::test]
    async fn executable_skill_frontmatter_run_field_is_parsed_and_distinguished_from_inert_skill() {
        let temp = tempfile::tempdir().unwrap();
        let skills_dir = temp.path().join(".rokr").join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        fs::write(
            skills_dir.join("executable.md"),
            "---\nrun: echo should-be-detected-as-executable\n---\nBody text.",
        )
        .unwrap();
        fs::write(
            skills_dir.join("inert.md"),
            "Just plain inert skill text, no frontmatter at all.",
        )
        .unwrap();

        let registry = CommandRegistry::discover_project_scope(temp.path());
        let workspace_root = temp.path().to_path_buf();
        let trust_store = fresh_trust_store(temp.path());
        let consent = RecordingConsentResolver::default();

        let inert_resolved = registry
            .resolve_skills(
                "@skill:inert",
                workspace_root.clone(),
                trust_store.clone(),
                consent.clone(),
            )
            .await
            .expect("resolving an inert skill mention should not error");
        assert_eq!(
            inert_resolved, "Just plain inert skill text, no frontmatter at all.",
            "expected an inert skill (no run: frontmatter) to inline its full raw contents \
             unchanged, exactly ticket 65's behavior"
        );
        assert!(
            !consent.was_asked(),
            "expected an inert skill mention to never consult the ConsentResolver"
        );

        let _ = registry
            .resolve_skills(
                "@skill:executable",
                workspace_root,
                trust_store,
                consent.clone(),
            )
            .await
            .expect("resolving a declined executable skill mention should not error");
        assert!(
            consent.was_asked(),
            "expected a skill with a run: frontmatter field to be recognized as executable and \
             trigger a consent check, distinguishing it from an inert skill"
        );
    }

    /// M-3 (pre-ship review): an empty or whitespace-only `run:` value
    /// (`run:` with nothing after it, or `run:    `) must parse as if
    /// there were no `run:` field at all -- NOT as `Some("")`, which would
    /// gate the skill behind a real consent prompt only to execute
    /// `sh -c ""` (a no-op) on approval.
    #[test]
    fn parse_skill_run_field_treats_empty_or_whitespace_value_as_absent() {
        assert_eq!(parse_skill_run_field("---\nrun:\n---\nBody."), None);
        assert_eq!(parse_skill_run_field("---\nrun:   \n---\nBody."), None);
        assert_eq!(
            parse_skill_run_field("---\nrun: echo hi\n---\nBody."),
            Some("echo hi".to_string()),
            "a genuinely non-empty run: value must still parse normally"
        );
    }

    /// M-3 (pre-ship review), end-to-end: a skill file with an empty
    /// `run:` value must resolve exactly like an inert skill (full body
    /// inlined, no consent prompt at all) -- confirms the parse-time guard
    /// actually prevents a pointless consent prompt for a command that
    /// would do nothing, not just that the parsed value looks right in
    /// isolation.
    #[tokio::test]
    async fn skill_with_empty_run_field_is_resolved_as_inert_without_consent_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let skills_dir = temp.path().join(".rokr").join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        // `Skill::contents` is always the file's raw, full, untouched text
        // (see `Skill`'s doc comment) -- an inert mention inlines this
        // verbatim, frontmatter block included, so that's exactly what
        // must come back here.
        let raw_contents = "---\nrun:\n---\nNoop skill body text.";
        fs::write(skills_dir.join("noop.md"), raw_contents).unwrap();

        let registry = CommandRegistry::discover_project_scope(temp.path());
        let trust_store = fresh_trust_store(temp.path());
        let consent = RecordingConsentResolver::default();

        let resolved = registry
            .resolve_skills(
                "@skill:noop",
                temp.path().to_path_buf(),
                trust_store,
                consent.clone(),
            )
            .await
            .expect("an empty-run: skill mention should not error");

        assert!(
            !consent.was_asked(),
            "expected an empty run: value to be treated as no run: field at all, never \
             consulting the ConsentResolver"
        );
        assert_eq!(
            resolved, raw_contents,
            "expected the skill's full raw contents to be inlined verbatim, exactly like an \
             inert skill"
        );
    }

    /// Load-bearing: the git-clone guard. A stub `ConsentResolver` that
    /// records whether it was asked, and declines, proves `resolve_skills`
    /// never executes the command and never bypasses the resolver.
    #[tokio::test]
    async fn untrusted_project_scope_executable_skill_does_not_run_without_consent() {
        let temp = tempfile::tempdir().unwrap();
        let skills_dir = temp.path().join(".rokr").join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        let marker_path = temp.path().join("marker-must-not-be-created");
        fs::write(
            skills_dir.join("deploy.md"),
            format!(
                "---\nrun: touch {}\n---\nDeploy body text.",
                marker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let registry = CommandRegistry::discover_project_scope(temp.path());
        let trust_store = fresh_trust_store(temp.path());
        let consent = RecordingConsentResolver::default();

        let resolved = registry
            .resolve_skills(
                "@skill:deploy",
                temp.path().to_path_buf(),
                trust_store,
                consent.clone(),
            )
            .await
            .expect("a declined skill mention should not error");

        assert!(
            consent.was_asked(),
            "expected the consent resolver to be consulted for an untrusted project-scope \
             executable skill"
        );
        assert!(
            !marker_path.exists(),
            "the run: command must never execute before consent is granted"
        );
        assert!(
            !resolved.contains("Deploy body text."),
            "a declined skill's body must never be inlined"
        );
    }

    /// A user-scope executable skill executes with a `ConsentResolver` stub
    /// that panics if ever consulted, proving scope alone (not a stored
    /// grant) is what exempts it.
    #[tokio::test]
    async fn user_scope_executable_skill_is_auto_trusted() {
        let temp = tempfile::tempdir().unwrap();
        let skills_dir = temp.path().join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        let marker_path = temp.path().join("user-scope-marker");
        fs::write(
            skills_dir.join("greet.md"),
            format!(
                "---\nrun: printf hello-user-scope && touch {}\n---\nGreet body.",
                marker_path.to_string_lossy()
            ),
        )
        .unwrap();

        let registry = CommandRegistry::discover_user_scope(temp.path());
        let trust_store = fresh_trust_store(temp.path());
        let consent = PanicsIfConsultedConsentResolver;

        let resolved = registry
            .resolve_skills(
                "@skill:greet",
                temp.path().to_path_buf(),
                trust_store,
                consent,
            )
            .await
            .expect("a user-scope executable skill should run without error");

        assert!(
            marker_path.exists(),
            "expected the user-scope skill's run: command to execute unconditionally, with no \
             consent check"
        );
        assert_eq!(
            resolved.trim(),
            "hello-user-scope",
            "expected the run: command's captured stdout to be inlined in place of the mention"
        );
    }

    /// On decline, the mention is replaced by a short notice string, and
    /// critically NOT the skill's markdown body (a declined skill's body
    /// may assume its command ran).
    #[tokio::test]
    async fn declined_consent_replaces_mention_with_not_executed_notice() {
        let temp = tempfile::tempdir().unwrap();
        let skills_dir = temp.path().join(".rokr").join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        fs::write(
            skills_dir.join("release.md"),
            "---\nrun: echo hi\n---\nASSUMES-COMMAND-ALREADY-RAN body text.",
        )
        .unwrap();

        let registry = CommandRegistry::discover_project_scope(temp.path());
        let trust_store = fresh_trust_store(temp.path());
        let consent = RecordingConsentResolver::default();

        let resolved = registry
            .resolve_skills(
                "Before @skill:release after",
                temp.path().to_path_buf(),
                trust_store,
                consent,
            )
            .await
            .expect("a declined skill mention should not error");

        assert!(
            !resolved.contains("ASSUMES-COMMAND-ALREADY-RAN"),
            "a declined skill's body (which may assume its command ran) must never be inlined, \
             got: {resolved:?}"
        );
        assert!(
            resolved.to_lowercase().contains("not executed"),
            "expected a short 'not executed' notice in place of the declined mention, got: \
             {resolved:?}"
        );
        assert!(
            resolved.contains("Before ") && resolved.contains(" after"),
            "expected the surrounding text to be preserved, got: {resolved:?}"
        );
    }

    /// A test `ConsentResolver` stub that always returns a fixed
    /// `ConsentOutcome`, regardless of the request -- used below to drive
    /// `resolve_skills`' `ApproveAndPersist` vs `ApproveWithoutPersisting`
    /// branches (commands.rs:357-365) directly, independent of
    /// `InteractiveConsentResolver`'s TUI-decision mapping (which is
    /// unit-tested on its own in `skill_trust.rs`, per F-002 pre-ship
    /// review -- `map_permission_decision` can't be exercised here since it
    /// requires a live `rokr_tui::PermissionHandle`).
    #[derive(Clone)]
    struct FixedConsentResolver {
        outcome: crate::skill_trust::ConsentOutcome,
    }

    impl crate::skill_trust::ConsentResolver for FixedConsentResolver {
        fn resolve(
            &self,
            _request: crate::skill_trust::SkillConsentRequest,
        ) -> impl std::future::Future<Output = crate::skill_trust::ConsentOutcome> + Send {
            let outcome = self.outcome;
            async move { outcome }
        }
    }

    /// F-002 (pre-ship review): a one-shot approval -- what
    /// `InteractiveConsentResolver` now maps `[y] run once` to -- must
    /// execute the skill's `run:` command WITHOUT ever writing a
    /// trust-store grant. Complements the `map_permission_decision` unit
    /// test in `skill_trust.rs` by proving the outcome it produces is
    /// wired correctly end-to-end through `resolve_skills`.
    #[tokio::test]
    async fn approve_without_persisting_executes_but_writes_no_trust_grant() {
        let temp = tempfile::tempdir().unwrap();
        let skills_dir = temp.path().join(".rokr").join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        let marker_path = temp.path().join("run-once-marker");
        let skill_path = skills_dir.join("deploy.md");
        let contents = format!(
            "---\nrun: touch {}\n---\nDeploy body.",
            marker_path.to_string_lossy()
        );
        fs::write(&skill_path, &contents).unwrap();
        let hash = crate::skill_trust::hash_skill_contents(&contents);

        let registry = CommandRegistry::discover_project_scope(temp.path());
        let trust_store = fresh_trust_store(temp.path());
        let consent = FixedConsentResolver {
            outcome: crate::skill_trust::ConsentOutcome::ApproveWithoutPersisting,
        };

        registry
            .resolve_skills(
                "@skill:deploy",
                temp.path().to_path_buf(),
                trust_store.clone(),
                consent,
            )
            .await
            .expect("a one-shot-approved skill mention should not error");

        assert!(
            marker_path.exists(),
            "expected the run: command to execute on a one-shot approval"
        );
        assert!(
            !trust_store.is_trusted(&skill_path, &hash),
            "expected 'run once' (ApproveWithoutPersisting) to NEVER write a trust-store grant"
        );
    }

    /// F-002 (pre-ship review): a persisted approval -- what
    /// `InteractiveConsentResolver` now maps `[r] trust this skill version`
    /// to -- must execute the skill's `run:` command AND write a
    /// trust-store grant for its exact `(path, hash)`.
    #[tokio::test]
    async fn approve_and_persist_executes_and_writes_trust_grant() {
        let temp = tempfile::tempdir().unwrap();
        let skills_dir = temp.path().join(".rokr").join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        let marker_path = temp.path().join("persist-marker");
        let skill_path = skills_dir.join("deploy.md");
        let contents = format!(
            "---\nrun: touch {}\n---\nDeploy body.",
            marker_path.to_string_lossy()
        );
        fs::write(&skill_path, &contents).unwrap();
        let hash = crate::skill_trust::hash_skill_contents(&contents);

        let registry = CommandRegistry::discover_project_scope(temp.path());
        let trust_store = fresh_trust_store(temp.path());
        let consent = FixedConsentResolver {
            outcome: crate::skill_trust::ConsentOutcome::ApproveAndPersist,
        };

        registry
            .resolve_skills(
                "@skill:deploy",
                temp.path().to_path_buf(),
                trust_store.clone(),
                consent,
            )
            .await
            .expect("a persisted-approval skill mention should not error");

        assert!(
            marker_path.exists(),
            "expected the run: command to execute on a persisted approval"
        );
        assert!(
            trust_store.is_trusted(&skill_path, &hash),
            "expected 'trust this skill version' (ApproveAndPersist) to write a trust-store grant"
        );
    }
}
