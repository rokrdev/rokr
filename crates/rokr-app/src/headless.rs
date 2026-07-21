//! Ticket 54 (headless-print-mode-text-output): headless (`-p`/`--print`)
//! mode selection. `select_mode` decides whether this invocation should
//! launch the TUI (the `-p`/`--print` flag absent) or run headless against a
//! single prompt (the flag present) -- and, when the flag's value is the
//! literal `-`, resolves that prompt by reading stdin instead of using `-`
//! itself as the prompt text.

/// Whether this invocation should launch the TUI or run headless against a
/// single prompt. See [`select_mode`].
pub enum Mode {
    /// No `-p`/`--print` flag: launch the TUI, unchanged from before this
    /// ticket.
    Tui,
    /// `-p`/`--print <prompt>` was given; run headless against this prompt
    /// text with no terminal UI.
    Headless(String),
}

/// Resolves the parsed `--print` flag value into a [`Mode`]. `print` is
/// `Cli::print` from `crate::cli` (`None` when the flag is absent). A value
/// of the literal `-` reads the prompt from `stdin` instead of using `-`
/// itself as the prompt text -- `stdin` is injected (rather than always
/// reading the real `std::io::stdin()`) so this is testable without a real
/// terminal or piped process.
pub fn select_mode(print: Option<&str>, mut stdin: impl std::io::Read) -> Mode {
    match print {
        None => Mode::Tui,
        Some("-") => {
            let mut buf = String::new();
            let _ = stdin.read_to_string(&mut buf);
            Mode::Headless(buf.trim_end().to_string())
        }
        Some(prompt) => Mode::Headless(prompt.to_string()),
    }
}

/// A permission surface that denies every gated-tool request, used to
/// drive `SessionRunner::run_submission` in headless mode. Headless forces
/// `AgentTier::Plan` (read-only: `read`/`glob`/`grep`/`ls` only -- none of
/// which are gated, see `crate::runner::SessionRunner`'s tool-set assembly),
/// so this is never actually invoked for this slice; kept as an explicit
/// deny (rather than e.g. an `unreachable!()` panic) as defense-in-depth
/// against a future headless slice that widens the tool tier without also
/// revisiting this default.
#[derive(Clone)]
pub struct DenyAllPermissions;

impl crate::runner::PermissionRequester for DenyAllPermissions {
    fn request(
        &self,
        _request: rokr_tui::PermissionRequest,
    ) -> impl std::future::Future<Output = bool> + Send {
        async { false }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `select_mode` must return `Mode::Tui` when `--print` is absent
    /// (today's unchanged TUI-launch behavior), and `Mode::Headless(prompt)`
    /// carrying the literal argument text when `--print <prompt>` is
    /// present.
    #[test]
    fn headless_mode_selected_only_when_print_flag_present_otherwise_tui_launches() {
        assert!(matches!(select_mode(None, std::io::empty()), Mode::Tui));

        match select_mode(Some("say hi"), std::io::empty()) {
            Mode::Headless(prompt) => assert_eq!(prompt, "say hi"),
            Mode::Tui => panic!("expected Headless mode when --print is present"),
        }
    }

    /// `--print -` must read the prompt from stdin rather than treating the
    /// literal `-` as the prompt text.
    #[test]
    fn dash_prompt_argument_reads_from_stdin_instead_of_argv() {
        let stdin = std::io::Cursor::new(b"say hi\n".to_vec());

        match select_mode(Some("-"), stdin) {
            Mode::Headless(prompt) => assert_eq!(prompt, "say hi"),
            Mode::Tui => panic!("expected Headless mode when --print - is present"),
        }
    }
}
