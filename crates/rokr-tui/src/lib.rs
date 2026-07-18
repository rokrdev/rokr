//! ratatui frontend: render loop, layout, input handling.

use std::io::{self, IsTerminal, Stdout};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};

/// Header block title, rendered at the top of the TUI.
pub const HEADER_TITLE: &str = "Header";
/// View block title, rendered in the flexible middle section.
pub const VIEW_TITLE: &str = "View";
/// Prompt block title, rendered at the bottom as the input line.
pub const PROMPT_TITLE: &str = "Prompt";

const HEADER_HEIGHT: u16 = 3;
const PROMPT_HEIGHT: u16 = 3;

/// State rendered into the TUI's View section. Owns the scrollback lines
/// shown in the View pane and the current prompt input buffer. Future
/// tickets (e.g. `single-turn-prompt`) push content into `view_lines` and
/// drive `prompt_input` from keystrokes.
#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub view_lines: Vec<String>,
    pub prompt_input: String,
}

/// Errors returned by [`run`].
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    /// stdout is not connected to a terminal, so the TUI cannot be drawn.
    #[error("rokr requires an interactive terminal (stdout is not a tty)")]
    NotATty,
    #[error("terminal io error: {0}")]
    Io(#[from] io::Error),
}

impl TuiError {
    /// True when this error is the "no tty available" case, as opposed to
    /// an unexpected terminal I/O failure.
    pub fn is_not_a_tty(&self) -> bool {
        matches!(self, TuiError::NotATty)
    }
}

/// Render `state` into `frame`, split top-to-bottom into Header (fixed
/// height), View (flexible), and Prompt (fixed height) sections. Pure
/// function — no I/O — so it is unit-testable against any ratatui backend,
/// including `TestBackend`.
pub fn draw(frame: &mut Frame, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(HEADER_HEIGHT),
            Constraint::Min(1),
            Constraint::Length(PROMPT_HEIGHT),
        ])
        .split(frame.area());

    let header = Block::default().borders(Borders::ALL).title(HEADER_TITLE);
    frame.render_widget(header, chunks[0]);

    let view = Paragraph::new(state.view_lines.join("\n"))
        .block(Block::default().borders(Borders::ALL).title(VIEW_TITLE));
    frame.render_widget(view, chunks[1]);

    let prompt = Paragraph::new(state.prompt_input.as_str())
        .block(Block::default().borders(Borders::ALL).title(PROMPT_TITLE));
    frame.render_widget(prompt, chunks[2]);
}

/// Ensures raw mode is disabled and the alternate screen is left, no matter
/// how the event loop exits (normal return, early `?`, or panic unwind).
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn restore_terminal() {
    // Best-effort: we're often already unwinding or exiting, so there's no
    // good way to react to failures here.
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

/// Installs a panic hook that restores the terminal (raw mode off, leave
/// alternate screen) before delegating to the previous hook, so a panic
/// mid-render doesn't leave the user's terminal corrupted.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        restore_terminal();
        previous(panic_info);
    }));
}

/// Runs the TUI event loop: draws `Header`/`View`/`Prompt`, and exits on
/// `q` or Ctrl+C, restoring the terminal on every exit path. Fails fast
/// with [`TuiError::NotATty`] if stdout isn't a terminal, rather than
/// corrupting non-interactive output.
pub async fn run() -> Result<(), TuiError> {
    if !io::stdout().is_terminal() {
        return Err(TuiError::NotATty);
    }

    tokio::task::spawn_blocking(run_blocking)
        .await
        .unwrap_or_else(|join_err| std::panic::resume_unwind(join_err.into_panic()))
}

fn run_blocking() -> Result<(), TuiError> {
    install_panic_hook();
    let _guard = TerminalGuard::enter()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut state = AppState::default();

    event_loop(&mut terminal, &mut state)
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
) -> Result<(), TuiError> {
    loop {
        terminal.draw(|frame| draw(frame, state))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press && should_quit(key.code, key.modifiers) {
                    return Ok(());
                }
            }
        }
    }
}

fn should_quit(code: KeyCode, modifiers: KeyModifiers) -> bool {
    matches!(code, KeyCode::Char('q'))
        || (matches!(code, KeyCode::Char('c')) && modifiers.contains(KeyModifiers::CONTROL))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn row_text(buffer: &Buffer, y: u16) -> String {
        let area = buffer.area;
        (area.x..area.x + area.width)
            .map(|x| {
                buffer
                    .cell((x, y))
                    .map(|cell| cell.symbol())
                    .unwrap_or(" ")
            })
            .collect()
    }

    fn find_title_row(buffer: &Buffer, title: &str) -> Option<u16> {
        let area = buffer.area;
        (area.y..area.y + area.height).find(|&y| row_text(buffer, y).contains(title))
    }

    #[test]
    fn layout_splits_into_header_view_prompt() {
        let backend = TestBackend::new(40, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = AppState::default();

        terminal.draw(|frame| draw(frame, &state)).unwrap();

        let buffer = terminal.backend().buffer();

        let header_row = find_title_row(buffer, "Header").expect("Header title not found");
        let view_row = find_title_row(buffer, "View").expect("View title not found");
        let prompt_row = find_title_row(buffer, "Prompt").expect("Prompt title not found");

        assert_eq!(header_row, 0, "Header should be the topmost section");
        assert!(
            header_row < view_row,
            "View should be below Header (header={header_row}, view={view_row})"
        );
        assert!(
            view_row < prompt_row,
            "Prompt should be below View (view={view_row}, prompt={prompt_row})"
        );
        assert_eq!(
            prompt_row,
            20 - 3,
            "Prompt should occupy the bottom fixed-height section"
        );
    }
}
