//! ratatui frontend: render loop, layout, input handling.

use std::future::Future;
use std::io::{self, IsTerminal, Stdout};
use std::sync::mpsc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tokio::sync::oneshot;

/// Header block title, rendered at the top of the TUI.
pub const HEADER_TITLE: &str = "Header";
/// View block title, rendered in the flexible middle section.
pub const VIEW_TITLE: &str = "View";
/// Prompt block title, rendered at the bottom as the input line.
pub const PROMPT_TITLE: &str = "Prompt";

const HEADER_HEIGHT: u16 = 3;
const PROMPT_HEIGHT: u16 = 3;

/// State rendered into the TUI's View section. Owns the scrollback lines
/// shown in the View pane and the current prompt input buffer.
#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub view_lines: Vec<String>,
    pub prompt_input: String,
    /// True while a submitted prompt is awaiting a response. Drives the
    /// pending indicator in [`draw`] and blocks new input/submission until
    /// the in-flight call resolves.
    pub pending: bool,
    /// Set while a gated tool call is awaiting the user's accept/deny
    /// decision. Drives the permission prompt in [`draw`] and, like
    /// `pending`, blocks prompt input/submission until resolved.
    pub permission_request: Option<PermissionRequest>,
}

/// A gated tool call awaiting user permission, described in primitives only
/// (no dependency on `rokr-core`'s specific payload enum — see this
/// module's `run` doc comment on staying decoupled from rokr-core/
/// rokr-provider types). `detail` is a description of the tool's effect,
/// e.g. the shell command for `bash`, or an old/new diff for `write`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub tool_name: String,
    pub detail: PermissionDetail,
}

/// The primitive detail of what a gated tool call would do, shown in the
/// permission prompt. Mirrors (but stays decoupled from, per this module's
/// other docs) rokr-core's `PermissionPayload` shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDetail {
    /// A single free-form description, e.g. the shell command for `bash`.
    Text(String),
    /// Old/new content for a `write`-style change, rendered as a
    /// line-level diff.
    Diff { old: String, new: String },
}

/// Handle for requesting permission mid-`submit`, round-tripped through the
/// render loop the same way the existing submit/response channel is (ADR
/// 0008: provider/tool I/O stays off the render thread). `run` passes a
/// fresh clone to each `submit` call; all clones share the same underlying
/// channel into the render loop.
#[derive(Clone)]
pub struct PermissionHandle {
    tx: mpsc::Sender<(PermissionRequest, oneshot::Sender<bool>)>,
}

impl PermissionHandle {
    /// Sends `request` to the render loop and waits for the user's
    /// accept/deny decision. Resolves to `false` (deny) if the render loop
    /// is gone, so a gated tool fails closed rather than silently running.
    pub async fn request(&self, request: PermissionRequest) -> bool {
        let (resp_tx, resp_rx) = oneshot::channel();
        if self.tx.send((request, resp_tx)).is_err() {
            return false;
        }
        resp_rx.await.unwrap_or(false)
    }
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

    let mut view_lines = state.view_lines.clone();
    if let Some(request) = &state.permission_request {
        match &request.detail {
            PermissionDetail::Text(text) => {
                view_lines.push(format!(
                    "permission needed: {} \"{}\"",
                    request.tool_name, text
                ));
            }
            PermissionDetail::Diff { old, new } => {
                view_lines.push(format!("permission needed: {}", request.tool_name));
                view_lines.extend(diff_lines(old, new));
            }
        }
        view_lines.push("[y] allow  [n] deny".to_string());
    } else if state.pending {
        view_lines.push("...".to_string());
    }
    // Wrapped so a long line (e.g. a bash command in a permission prompt)
    // doesn't get silently clipped at the pane's width.
    let view = Paragraph::new(view_lines.join("\n"))
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(VIEW_TITLE));
    frame.render_widget(view, chunks[1]);

    let prompt = Paragraph::new(state.prompt_input.as_str())
        .block(Block::default().borders(Borders::ALL).title(PROMPT_TITLE));
    frame.render_widget(prompt, chunks[2]);
}

/// Renders `old` and `new` as a naive line-level diff: every line of `old`
/// prefixed `-`, every line of `new` prefixed `+`. Not a minimal/LCS diff —
/// deliberately simple per the ticket ("no new diff crate, plain old/new
/// line comparison").
fn diff_lines(old: &str, new: &str) -> Vec<String> {
    old.lines()
        .map(|line| format!("-{line}"))
        .chain(new.lines().map(|line| format!("+{line}")))
        .collect()
}

/// Ensures raw mode is disabled and the alternate screen is left, no matter
/// how the event loop exits (normal return, early `?`, or panic unwind).
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        // Construct the guard before the fallible `execute!` below: if entering
        // the alternate screen fails, the guard still gets dropped on the way
        // out and disables raw mode instead of leaking it.
        let guard = Self;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(guard)
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
/// `q` (when the prompt is empty) or Ctrl+C (always), restoring the
/// terminal on every exit path. Fails fast with [`TuiError::NotATty`] if
/// stdout isn't a terminal, rather than corrupting non-interactive output.
///
/// `submit` is called with the prompt text and a [`PermissionHandle`]
/// whenever the user presses Enter on a non-empty prompt; it is
/// deliberately generic (rather than depending on `rokr-core`/
/// `rokr-provider` types) so this crate stays decoupled from the message
/// model and provider abstraction — the caller (typically `rokr`'s
/// `main.rs`) wires those in, bridging its own permission-request type to
/// [`PermissionRequest`] via the handle. The call runs on a spawned tokio
/// task so the blocking crossterm event loop never stalls waiting on it;
/// [`AppState::pending`] reflects the in-flight state in the meantime, and
/// [`AppState::permission_request`] reflects a gated tool call awaiting the
/// user's decision mid-`submit`.
pub async fn run<F, Fut>(submit: F) -> Result<(), TuiError>
where
    F: Fn(String, PermissionHandle) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<String, String>> + Send + 'static,
{
    if !io::stdout().is_terminal() {
        return Err(TuiError::NotATty);
    }

    let handle = tokio::runtime::Handle::current();

    tokio::task::spawn_blocking(move || run_blocking(handle, submit))
        .await
        .unwrap_or_else(|join_err| std::panic::resume_unwind(join_err.into_panic()))
}

fn run_blocking<F, Fut>(handle: tokio::runtime::Handle, submit: F) -> Result<(), TuiError>
where
    F: Fn(String, PermissionHandle) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<String, String>> + Send + 'static,
{
    install_panic_hook();
    let _guard = TerminalGuard::enter()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut state = AppState::default();

    event_loop(&mut terminal, &mut state, &handle, &submit)
}

fn event_loop<F, Fut>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    handle: &tokio::runtime::Handle,
    submit: &F,
) -> Result<(), TuiError>
where
    F: Fn(String, PermissionHandle) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<String, String>> + Send + 'static,
{
    // Carries the outcome of a spawned `submit` call back into the blocking
    // event loop without blocking it: each iteration does a non-blocking
    // `try_recv` alongside the existing crossterm poll.
    let (tx, rx) = mpsc::channel::<Result<String, String>>();
    // Carries permission requests from a spawned `submit` call into the
    // render loop the same way, paired with a oneshot the render loop uses
    // to send the user's decision back out to the awaiting `submit` call.
    let (perm_tx, perm_rx) = mpsc::channel::<(PermissionRequest, oneshot::Sender<bool>)>();
    // The responder for whichever request `state.permission_request`
    // currently holds. Kept outside `AppState` because `oneshot::Sender`
    // isn't `Clone`/`Debug`, which `AppState`'s derives require.
    let mut pending_permission_responder: Option<oneshot::Sender<bool>> = None;
    // ADR 0008: redraw only on state change, never on a fixed timer with no
    // change. Starts true so the first frame still paints.
    let mut dirty = true;

    loop {
        if dirty {
            terminal.draw(|frame| draw(frame, state))?;
            dirty = false;
        }

        if let Ok((request, responder)) = perm_rx.try_recv() {
            state.permission_request = Some(request);
            pending_permission_responder = Some(responder);
            dirty = true;
        }

        if let Ok(outcome) = rx.try_recv() {
            state.pending = false;
            match outcome {
                Ok(response) => state.view_lines.push(response),
                Err(error) => state.view_lines.push(format!("Error: {error}")),
            }
            dirty = true;
        }

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }

                    if should_quit(key.code, key.modifiers, state.prompt_input.is_empty()) {
                        return Ok(());
                    }

                    if let Some(responder) = pending_permission_responder.take() {
                        match key.code {
                            KeyCode::Char('y') => {
                                let _ = responder.send(true);
                                state.permission_request = None;
                                dirty = true;
                            }
                            KeyCode::Char('n') => {
                                if let Some(request) = &state.permission_request {
                                    state
                                        .view_lines
                                        .push(format!("{}: permission denied", request.tool_name));
                                }
                                let _ = responder.send(false);
                                state.permission_request = None;
                                dirty = true;
                            }
                            _ => {
                                // Not a decision key: put the responder back
                                // and keep waiting, same as ignoring any
                                // other key while `pending` is true below.
                                pending_permission_responder = Some(responder);
                            }
                        }
                        continue;
                    }

                    if state.pending {
                        continue;
                    }

                    match key.code {
                        KeyCode::Enter if !state.prompt_input.is_empty() => {
                            let input = std::mem::take(&mut state.prompt_input);
                            state.view_lines.push(format!("> {input}"));
                            state.pending = true;
                            dirty = true;

                            let permission = PermissionHandle {
                                tx: perm_tx.clone(),
                            };
                            let submit_fut = submit(input, permission);
                            let tx = tx.clone();
                            handle.spawn(async move {
                                let outcome = submit_fut.await;
                                let _ = tx.send(outcome);
                            });
                        }
                        KeyCode::Char(c) => {
                            state.prompt_input.push(c);
                            dirty = true;
                        }
                        KeyCode::Backspace => {
                            state.prompt_input.pop();
                            dirty = true;
                        }
                        _ => {}
                    }
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }
    }
}

/// `q` quits only when the prompt is empty, so it doesn't clash with typing
/// a prompt that uses the letter; Ctrl+C always quits as a hard escape
/// hatch regardless of prompt content.
fn should_quit(code: KeyCode, modifiers: KeyModifiers, prompt_is_empty: bool) -> bool {
    (matches!(code, KeyCode::Char('q')) && prompt_is_empty)
        || (matches!(code, KeyCode::Char('c')) && modifiers.contains(KeyModifiers::CONTROL))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    fn row_text(buffer: &Buffer, y: u16) -> String {
        let area = buffer.area;
        (area.x..area.x + area.width)
            .map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" "))
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
