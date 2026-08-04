//! Ticket 52 (clap-and-sessionrunner-extraction): the `clap` (derive) CLI
//! surface, replacing `main.rs`'s hand-rolled `extract_resume_mode` /
//! `parse_agent_tier` / positional arg matching. Parsing the same surface
//! today's `USAGE` string documented -- `--version`, `--agent <plan|build>`,
//! `--resume <id>`, `--continue`, and the `auth login` subcommand -- but now
//! through a single declarative struct that also drives `--help`.

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

/// The agent's tool tier, selected via `--agent` and defaulting to `Plan`
/// when no flag is given (the caller applies that default;
/// [`Cli::agent`] is `None` when the flag is absent). `Plan` is read-only:
/// read/glob/grep/ls only, so the agent can explore and reason about a
/// codebase without being able to change anything. `Build` adds
/// bash/write/edit on top, unlocking actual mutation. Each tier's tools are
/// all wired through the same `rokr_core::run_tool_loop`; the tier only
/// changes which tools are handed in and which system prompt
/// (`{config_dir}/agents/{tier}.md`) is seeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum AgentTier {
    Plan,
    Build,
}

impl AgentTier {
    pub fn prompt_name(self) -> &'static str {
        match self {
            AgentTier::Plan => "plan",
            AgentTier::Build => "build",
        }
    }
}

/// Ticket 35 (resume-session): which prior session (if any) this run should
/// resume into, derived from the parsed `--resume <id>` / `--continue`
/// flags via [`Cli::resume_mode`].
pub enum ResumeMode {
    None,
    Id(String),
    Continue,
}

/// Ticket 55 (headless-output-formats-and-permission-mode): the headless
/// `--output-format` flag. `Text` (the default, absent selects it) is
/// today's unchanged "print only the final assistant text" behavior from
/// ticket 54. `Json` prints one `ResultObject` (`crate::result_schema`).
/// `StreamJson` prints JSONL events followed by that same object. Only
/// meaningful in headless mode (`-p`/`--print`); ignored by the TUI path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

/// Ticket 55 (headless-output-formats-and-permission-mode): the headless
/// `--permission-mode` flag, defaulting to `Deny` when absent (the caller
/// applies that default, matching [`Cli::agent`]'s existing pattern) -- a
/// gated tool call is denied unless the operator opts in. `AcceptEdits`
/// grants only write/edit (`Diff`) calls, still denying `bash` and MCP
/// tool calls (no human is present in headless to approve those
/// interactively). `Bypass` grants every gated call unconditionally, and
/// additionally requires `--dangerously-skip-permissions` (see
/// [`crate::headless::build_permission_requester`]) -- it cannot be reached
/// by this flag alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum PermissionMode {
    Deny,
    AcceptEdits,
    Bypass,
}

/// Ticket 60 (eval-report-json-and-ci-gate): the `rokr eval --report` flag.
/// `Text` (the default, absent selects it -- the caller applies that
/// default, mirroring `OutputFormat`'s existing pattern) is today's
/// unchanged per-case `PASS <name>`/`FAIL <name>` human output, still
/// exiting nonzero if any case failed. `Json` instead prints one aggregate
/// [`crate`] report (see `rokr_eval::report::Report`) and exits according
/// to `--pass-threshold` (a threshold comparison on the aggregate
/// deterministic pass rate, never exact-match on any single case) instead
/// of any-case-failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ReportFormat {
    Text,
    Json,
}

/// The full `rokr` command-line surface.
#[derive(Parser, Debug)]
#[command(
    name = "rokr",
    version,
    about = "rokr: a terminal coding agent",
    long_about = None
)]
pub struct Cli {
    /// The agent tool tier: `plan` (read-only) or `build` (adds mutation).
    /// Absent selects `plan` (the caller applies that default).
    #[arg(long)]
    pub agent: Option<AgentTier>,

    /// Resume a specific prior session by id.
    #[arg(long, value_name = "id")]
    pub resume: Option<String>,

    /// Resume the most recent prior session.
    #[arg(long = "continue")]
    pub continue_session: bool,

    /// Run headless: send this single prompt to the agent, print only the
    /// final assistant text to stdout, and exit -- no TUI. A value of `-`
    /// reads the prompt from stdin instead of using `-` itself as the
    /// prompt text. Absent, this launches the TUI unchanged (ticket 54:
    /// headless-print-mode-text-output).
    #[arg(short = 'p', long = "print", value_name = "prompt")]
    pub print: Option<String>,

    /// Headless-only (see `print` above): how to print the run's outcome.
    /// Absent selects `text` (the caller applies that default, matching
    /// `agent`'s existing pattern) -- today's unchanged behavior.
    #[arg(long = "output-format", value_enum)]
    pub output_format: Option<OutputFormat>,

    /// Headless-only: how a gated tool call (bash/write/edit/MCP) is
    /// decided with no human present. Absent selects `deny` (the caller
    /// applies that default) -- a gated tool call is denied unless the
    /// operator opts in.
    #[arg(long = "permission-mode", value_enum)]
    pub permission_mode: Option<PermissionMode>,

    /// Required alongside `--permission-mode bypass` to actually grant
    /// every gated tool call unconditionally; see
    /// `crate::headless::build_permission_requester`.
    #[arg(long)]
    pub dangerously_skip_permissions: bool,

    /// CI-friendly pre-approval for a project-scope executable skill's
    /// `run:` command (ADR 0018 decision 4's `--allow-skill` flag, deferred
    /// there, implemented here), bypassing the interactive TOFU consent
    /// prompt. Repeatable (`--allow-skill a --allow-skill b`). Each value is
    /// either a bare skill name (pre-approves regardless of the skill
    /// file's content hash) or `name@<sha256-hex>` (pre-approves ONLY when
    /// the skill file's content hash -- the same hash the trust store
    /// itself pins to -- matches exactly; a mismatch falls back to normal
    /// consent, never silent approval). Malformed values (a hash pin that
    /// isn't exactly 64 lowercase hex characters, or an empty name) are
    /// rejected at parse time. Never writes a `SkillTrustStore` entry -- the
    /// approval is ephemeral, same spirit as the interactive "[y] run once"
    /// path. Honored in both the TUI and headless paths; a no-op for
    /// user-scope skills, which are already auto-trusted.
    #[arg(long = "allow-skill", value_name = "name[@sha256]")]
    pub allow_skill: Vec<crate::skill_trust::AllowSkillEntry>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Top-level subcommands. Today there are two: `auth`, whose sole action is
/// `login` -- i.e. `rokr auth login`, the OAuth PKCE flow that runs and
/// exits rather than entering the TUI -- and `completions` (ticket 53:
/// shell-completions-subcommand), which prints a shell completion script to
/// stdout and exits rather than entering the TUI.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Authentication commands (`auth login`).
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Print a shell completion script to stdout (`completions <shell>`).
    Completions {
        /// Which shell's completion script to generate.
        shell: clap_complete::Shell,
    },
    /// Check for and apply an update to the `rokr` binary itself, or
    /// (Homebrew-managed installs) direct the user to `brew upgrade`
    /// instead (`upgrade`). Ticket 67 (self-update-rokr-upgrade).
    Upgrade,
    // Ticket 58 (eval-case-runner-and-deterministic-assertions): each case
    // is isolated in a fresh temp fixture dir, a fresh headless session,
    // and an explicit pinned model/permission mode -- no case inherits
    // ambient config or another case's state. Delegates to
    // `rokr_eval::run_eval`.
    /// Run every eval case file under `cases_dir`, reporting pass/fail per
    /// case (`eval <cases-dir>`).
    Eval {
        /// Directory containing eval case files (`*.json`).
        cases_dir: std::path::PathBuf,

        /// Required for a case file requesting `permission_mode: bypass` to
        /// actually be honored -- mirrors the top-level
        /// `Cli::dangerously_skip_permissions` flag (see its doc comment):
        /// a case file is data, not operator intent, so it must never be
        /// able to grant itself the equivalent of
        /// `--dangerously-skip-permissions` on its own. Without this flag, a
        /// bypass-requesting case fails with a clear error instead of
        /// running.
        #[arg(long)]
        dangerously_skip_permissions: bool,

        /// Ticket 60 (eval-report-json-and-ci-gate): how to report the
        /// run's outcome. Absent selects `text` (the caller applies that
        /// default, matching `output_format`'s existing pattern) -- today's
        /// unchanged PASS/FAIL-per-case printing and any-case-failed exit
        /// code. `json` instead prints one aggregate report and exits
        /// according to `--pass-threshold` below.
        #[arg(long = "report", value_enum)]
        report: Option<ReportFormat>,

        /// Only meaningful with `--report json` (see `report` above): the
        /// aggregate deterministic pass-rate threshold (0.0-1.0) the JSON
        /// report's exit code gates on -- `pass_rate >= pass_threshold`
        /// exits 0, otherwise nonzero (a threshold comparison, never
        /// exact-match on any single case -- see `rokr_eval::report::Report::exit_code`).
        /// ASSUMPTION: the ticket does not state a default, so this
        /// defaults to `1.0` (every case must pass) -- the closest
        /// equivalent to the `--report text`/absent path's existing
        /// any-case-failed exit-code semantics.
        #[arg(long = "pass-threshold", default_value_t = 1.0)]
        pass_threshold: f64,
    },
}

/// Ticket 53 (shell-completions-subcommand): renders the full [`Cli`]
/// command surface (`clap_complete::generate` walks it via
/// [`clap::CommandFactory`], so this stays in sync with `Cli`/`Command`
/// automatically -- no separate list of subcommands to hand-maintain) into a
/// completion script for the given `shell`. Used by both the
/// `completions_subcommand_generates_script_for_each_supported_shell` unit
/// test below and `main.rs`'s `Some(Command::Completions { shell })` dispatch
/// arm.
pub fn completions_script(shell: clap_complete::Shell) -> String {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    let mut buf = Vec::new();
    clap_complete::generate(shell, &mut cmd, name, &mut buf);
    String::from_utf8(buf).expect("clap_complete output should always be valid UTF-8")
}

/// Actions under the `auth` subcommand.
#[derive(Subcommand, Debug)]
pub enum AuthAction {
    /// Run the OAuth PKCE login flow and store the resulting token.
    Login,
}

impl Cli {
    /// Resolves the parsed `--resume` / `--continue` flags into a
    /// [`ResumeMode`]. `--resume <id>` (an explicit target) takes
    /// precedence over `--continue` (the most recent session) when both are
    /// somehow supplied -- an unusual combination the pre-clap hand-rolled
    /// parser resolved by "last flag on the command line wins" (position
    /// dependent), which clap cannot reproduce without extra machinery;
    /// realistic single-flag usage is identical either way.
    pub fn resume_mode(&self) -> ResumeMode {
        match (&self.resume, self.continue_session) {
            (Some(id), _) => ResumeMode::Id(id.clone()),
            (None, true) => ResumeMode::Continue,
            (None, false) => ResumeMode::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clap CLI must parse the same surface the hand-rolled
    /// `parse_agent_tier` / `extract_resume_mode` matcher used to:
    /// `--agent plan|build` (absent => the caller's `Plan` default),
    /// `--resume <id>` / `--continue` in any position, and the literal
    /// `auth login` subcommand -- while rejecting a bogus `--agent` value.
    #[test]
    fn clap_cli_parses_agent_resume_continue_flags_matching_hand_rolled_parser() {
        // No args: no tier flag (caller defaults to Plan), no resume, no
        // subcommand -- the pre-clap "empty args => Plan, enter TUI" path.
        let cli = Cli::try_parse_from(["rokr"]).expect("no-arg parse should succeed");
        assert!(cli.agent.is_none(), "absent --agent must parse as None");
        assert!(matches!(cli.resume_mode(), ResumeMode::None));
        assert!(cli.command.is_none());

        // `--agent build` selects the Build tier.
        let cli =
            Cli::try_parse_from(["rokr", "--agent", "build"]).expect("--agent build should parse");
        assert!(matches!(cli.agent, Some(AgentTier::Build)));
        assert!(matches!(cli.resume_mode(), ResumeMode::None));

        // `--agent plan` selects the Plan tier explicitly.
        let cli =
            Cli::try_parse_from(["rokr", "--agent", "plan"]).expect("--agent plan should parse");
        assert!(matches!(cli.agent, Some(AgentTier::Plan)));

        // `--resume <id>` resolves to ResumeMode::Id, carrying the id.
        let cli = Cli::try_parse_from(["rokr", "--resume", "01ABCDEF"])
            .expect("--resume <id> should parse");
        match cli.resume_mode() {
            ResumeMode::Id(id) => assert_eq!(id, "01ABCDEF"),
            _ => panic!("expected ResumeMode::Id"),
        }

        // `--continue` resolves to ResumeMode::Continue.
        let cli = Cli::try_parse_from(["rokr", "--continue"]).expect("--continue should parse");
        assert!(matches!(cli.resume_mode(), ResumeMode::Continue));

        // Resume flags compose with `--agent` in any position, exactly as
        // the old `extract_resume_mode` (which pulled them out of argv
        // before the tier match) allowed.
        let cli = Cli::try_parse_from(["rokr", "--resume", "01XYZ", "--agent", "build"])
            .expect("--resume <id> --agent build should parse");
        assert!(matches!(cli.agent, Some(AgentTier::Build)));
        match cli.resume_mode() {
            ResumeMode::Id(id) => assert_eq!(id, "01XYZ"),
            _ => panic!("expected ResumeMode::Id"),
        }

        // `auth login` parses as the Auth/Login subcommand (the same
        // literal positional pair the old `[a, b] if a == "auth" && b ==
        // "login"` arm matched), NOT as a flag.
        let cli = Cli::try_parse_from(["rokr", "auth", "login"]).expect("auth login should parse");
        assert!(matches!(
            cli.command,
            Some(Command::Auth {
                action: AuthAction::Login
            })
        ));

        // A bogus `--agent` value is a usage error, matching the old
        // `parse_agent_tier` `Err(())` path.
        assert!(
            Cli::try_parse_from(["rokr", "--agent", "bogus"]).is_err(),
            "an unknown --agent value must be a parse error"
        );
    }

    /// Ticket 55 (headless-output-formats-and-permission-mode): the three
    /// new headless-only flags must parse with the exact values the ticket
    /// documents -- `--output-format text|json|stream-json`,
    /// `--permission-mode deny|accept-edits|bypass`, and the bare
    /// `--dangerously-skip-permissions` bool flag -- and be absent (`None`
    /// / `false`) when not passed, matching `agent`'s existing
    /// caller-applies-the-default pattern.
    #[test]
    fn output_format_permission_mode_and_dangerously_skip_permissions_flags_parse() {
        let cli = Cli::try_parse_from(["rokr", "-p", "hi"]).expect("no-flag parse should succeed");
        assert!(
            cli.output_format.is_none(),
            "absent --output-format must parse as None"
        );
        assert!(
            cli.permission_mode.is_none(),
            "absent --permission-mode must parse as None"
        );
        assert!(!cli.dangerously_skip_permissions);

        let cli = Cli::try_parse_from(["rokr", "-p", "hi", "--output-format", "json"])
            .expect("--output-format json should parse");
        assert!(matches!(cli.output_format, Some(OutputFormat::Json)));

        let cli = Cli::try_parse_from(["rokr", "-p", "hi", "--output-format", "stream-json"])
            .expect("--output-format stream-json should parse");
        assert!(matches!(cli.output_format, Some(OutputFormat::StreamJson)));

        let cli = Cli::try_parse_from(["rokr", "-p", "hi", "--output-format", "text"])
            .expect("--output-format text should parse");
        assert!(matches!(cli.output_format, Some(OutputFormat::Text)));

        let cli = Cli::try_parse_from(["rokr", "-p", "hi", "--permission-mode", "accept-edits"])
            .expect("--permission-mode accept-edits should parse");
        assert!(matches!(
            cli.permission_mode,
            Some(PermissionMode::AcceptEdits)
        ));

        let cli = Cli::try_parse_from([
            "rokr",
            "-p",
            "hi",
            "--permission-mode",
            "bypass",
            "--dangerously-skip-permissions",
        ])
        .expect("--permission-mode bypass --dangerously-skip-permissions should parse");
        assert!(matches!(cli.permission_mode, Some(PermissionMode::Bypass)));
        assert!(cli.dangerously_skip_permissions);

        assert!(
            Cli::try_parse_from(["rokr", "-p", "hi", "--output-format", "bogus"]).is_err(),
            "an unknown --output-format value must be a parse error"
        );
        assert!(
            Cli::try_parse_from(["rokr", "-p", "hi", "--permission-mode", "bogus"]).is_err(),
            "an unknown --permission-mode value must be a parse error"
        );
    }

    /// Ticket 60 (eval-report-json-and-ci-gate): `--report text|json` and
    /// `--pass-threshold <N>` on `rokr eval` -- not one of the ticket's
    /// three named failing tests, but kept for parity with every other flag
    /// in this file having its own parse coverage. Absent `--report` parses
    /// as `None` (caller defaults to `Text`, matching `output_format`'s
    /// existing pattern) and absent `--pass-threshold` defaults to `1.0`
    /// (this crate's documented assumption -- see `Command::Eval`'s
    /// `pass_threshold` doc comment).
    #[test]
    fn report_and_pass_threshold_flags_parse_on_eval_command() {
        let cli = Cli::try_parse_from(["rokr", "eval", "cases"])
            .expect("no-flag eval parse should succeed");
        match cli.command {
            Some(Command::Eval {
                report,
                pass_threshold,
                ..
            }) => {
                assert!(report.is_none(), "absent --report must parse as None");
                assert_eq!(
                    pass_threshold, 1.0,
                    "absent --pass-threshold must default to 1.0"
                );
            }
            other => panic!("expected Command::Eval, got: {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "rokr",
            "eval",
            "cases",
            "--report",
            "json",
            "--pass-threshold",
            "0.8",
        ])
        .expect("--report json --pass-threshold 0.8 should parse");
        match cli.command {
            Some(Command::Eval {
                report,
                pass_threshold,
                ..
            }) => {
                assert!(matches!(report, Some(ReportFormat::Json)));
                assert_eq!(pass_threshold, 0.8);
            }
            other => panic!("expected Command::Eval, got: {other:?}"),
        }

        let cli = Cli::try_parse_from(["rokr", "eval", "cases", "--report", "text"])
            .expect("--report text should parse");
        match cli.command {
            Some(Command::Eval { report, .. }) => {
                assert!(matches!(report, Some(ReportFormat::Text)))
            }
            other => panic!("expected Command::Eval, got: {other:?}"),
        }

        assert!(
            Cli::try_parse_from(["rokr", "eval", "cases", "--report", "bogus"]).is_err(),
            "an unknown --report value must be a parse error"
        );
    }

    /// ADR 0018 decision 4's `--allow-skill` flag (deferred there,
    /// implemented here): repeatable, absent parses as an empty `Vec`
    /// (matching this file's caller-applies-the-default pattern for other
    /// flags), a bare name and `name@<64-lowercase-hex>` both parse, and a
    /// malformed value (a short/non-hex/uppercase-hex pin, or an empty name)
    /// is rejected by clap itself at arg-parse time -- not deferred to
    /// runtime.
    #[test]
    fn allow_skill_flag_is_repeatable_and_rejects_malformed_values_at_parse_time() {
        let cli = Cli::try_parse_from(["rokr", "-p", "hi"]).expect("no-flag parse should succeed");
        assert!(
            cli.allow_skill.is_empty(),
            "absent --allow-skill must parse as an empty Vec"
        );

        let hash = "a".repeat(64);
        let cli = Cli::try_parse_from([
            "rokr",
            "-p",
            "hi",
            "--allow-skill",
            "deploy",
            "--allow-skill",
            &format!("release@{hash}"),
        ])
        .expect("two --allow-skill occurrences should parse");
        assert_eq!(cli.allow_skill.len(), 2, "expected both occurrences to be collected");
        assert_eq!(cli.allow_skill[0].name, "deploy");
        assert_eq!(cli.allow_skill[0].hash, None);
        assert_eq!(cli.allow_skill[1].name, "release");
        assert_eq!(cli.allow_skill[1].hash, Some(hash));

        assert!(
            Cli::try_parse_from(["rokr", "-p", "hi", "--allow-skill", "deploy@short"]).is_err(),
            "a hash pin that isn't 64 hex characters must be a parse error"
        );
        assert!(
            Cli::try_parse_from([
                "rokr",
                "-p",
                "hi",
                "--allow-skill",
                &format!("deploy@{}", "G".repeat(64)),
            ])
            .is_err(),
            "a non-hex pin must be a parse error"
        );
        assert!(
            Cli::try_parse_from([
                "rokr",
                "-p",
                "hi",
                "--allow-skill",
                &format!("deploy@{}", "A".repeat(64)),
            ])
            .is_err(),
            "an uppercase-hex pin must be a parse error -- only lowercase hex is accepted"
        );
        assert!(
            Cli::try_parse_from(["rokr", "-p", "hi", "--allow-skill", "@abc123"]).is_err(),
            "an empty skill name before '@' must be a parse error"
        );
        assert!(
            Cli::try_parse_from(["rokr", "-p", "hi", "--allow-skill", ""]).is_err(),
            "an empty --allow-skill value must be a parse error"
        );
    }

    /// Ticket 53 (shell-completions-subcommand) RED: `completions_script`
    /// (backed by `clap_complete::generate`) must produce a non-empty
    /// completion script for each shell clap_complete supports selecting via
    /// `rokr completions <shell>`, and that script must mention every real
    /// top-level subcommand this codebase actually has today -- `auth` and
    /// `completions` itself (NOT `eval`/`upgrade`, which don't exist yet).
    /// Fails to compile today since neither `Command::Completions` nor
    /// `completions_script` exist yet.
    #[test]
    fn completions_subcommand_generates_script_for_each_supported_shell() {
        for shell in [
            clap_complete::Shell::Zsh,
            clap_complete::Shell::Bash,
            clap_complete::Shell::Fish,
        ] {
            let script = completions_script(shell);
            assert!(
                !script.is_empty(),
                "expected a non-empty completion script for {shell:?}"
            );
            assert!(
                script.contains("auth"),
                "expected {shell:?} completion script to mention the `auth` subcommand, got: {script}"
            );
            assert!(
                script.contains("completions"),
                "expected {shell:?} completion script to mention the `completions` subcommand, got: {script}"
            );
        }
    }
}
