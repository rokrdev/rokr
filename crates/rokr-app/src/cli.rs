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
        let cli = Cli::try_parse_from(["rokr", "--agent", "build"])
            .expect("--agent build should parse");
        assert!(matches!(cli.agent, Some(AgentTier::Build)));
        assert!(matches!(cli.resume_mode(), ResumeMode::None));

        // `--agent plan` selects the Plan tier explicitly.
        let cli = Cli::try_parse_from(["rokr", "--agent", "plan"])
            .expect("--agent plan should parse");
        assert!(matches!(cli.agent, Some(AgentTier::Plan)));

        // `--resume <id>` resolves to ResumeMode::Id, carrying the id.
        let cli = Cli::try_parse_from(["rokr", "--resume", "01ABCDEF"])
            .expect("--resume <id> should parse");
        match cli.resume_mode() {
            ResumeMode::Id(id) => assert_eq!(id, "01ABCDEF"),
            _ => panic!("expected ResumeMode::Id"),
        }

        // `--continue` resolves to ResumeMode::Continue.
        let cli =
            Cli::try_parse_from(["rokr", "--continue"]).expect("--continue should parse");
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
        let cli =
            Cli::try_parse_from(["rokr", "auth", "login"]).expect("auth login should parse");
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
