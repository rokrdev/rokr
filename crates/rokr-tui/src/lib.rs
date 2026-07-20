//! ratatui frontend: render loop, layout, input handling.

use std::future::Future;
use std::io::{self, IsTerminal, Stdout};
use std::process::{Command, ExitStatus};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
/// Caps how many lines of a `write`-style diff are shown in the permission
/// prompt before being truncated with a "(N more lines)" marker, so a huge
/// diff can't push the prompt itself further off the bottom of the View
/// pane than the scroll-to-bottom logic in [`draw`] already has to account
/// for.
const MAX_DIFF_LINES: usize = 18;

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
    /// Ticket 40 (prompt-history): every submitted prompt (both `Submit`
    /// and `Command` routes), oldest-first -- seeded at startup from
    /// `rokr_session::PromptHistory::load` (via `run`'s `history`
    /// parameter; rokr-tui never depends on rokr-session itself, just this
    /// primitive `Vec<String>`) and grown in-memory as further prompts are
    /// submitted this run, so Up/Down recall sees this run's own
    /// submissions immediately, not only ones persisted before this
    /// process started.
    pub history: Vec<String>,
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
    let showing_permission_prompt = state.permission_request.is_some();
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
                view_lines.extend(truncate_diff(diff_lines(old, new)));
            }
        }
        view_lines.push("[y] allow  [n] deny".to_string());
    } else if state.pending {
        view_lines.push("...".to_string());
    }
    // While a permission prompt is showing, anchor the Paragraph's scroll to
    // the bottom so the prompt + "[y]/[n]" line stay visible even when a
    // long transcript (or, previously, a long diff — now capped by
    // MAX_DIFF_LINES) would otherwise push them past the bottom of the View
    // pane. `Paragraph` has no bottom-anchor mode, so this computes an
    // explicit `scroll` offset instead.
    //
    // Known limitation: this counts *unwrapped* logical lines against the
    // pane's inner height, not post-wrap rendered rows, since wrapping
    // depends on each line's content and isn't known until ratatui lays the
    // paragraph out. Embedded newlines within a `view_lines` entry (e.g. a
    // multi-paragraph model response pushed as a single entry) are counted
    // exactly via `split('\n')` below, since `Paragraph::join("\n")` turns
    // each one into a real rendered row regardless of pane width. Only
    // *wrapping* of a long single logical line remains an approximation; a
    // very long unwrapped line could still make the estimate short.
    let scroll_y = if showing_permission_prompt {
        let inner_height = chunks[1].height.saturating_sub(2); // top/bottom border
        let total_lines: usize = view_lines.iter().map(|line| line.split('\n').count()).sum();
        (total_lines as u16).saturating_sub(inner_height)
    } else {
        0
    };
    // Wrapped so a long line (e.g. a bash command in a permission prompt)
    // doesn't get silently clipped at the pane's width.
    let view = Paragraph::new(view_lines.join("\n"))
        .wrap(Wrap { trim: false })
        .scroll((scroll_y, 0))
        .block(Block::default().borders(Borders::ALL).title(VIEW_TITLE));
    frame.render_widget(view, chunks[1]);

    // Ticket 42 (editor-integration) fix: bottom-anchor the Prompt pane the
    // same way the View pane already does above, so a multi-line buffer
    // (composed via Alt/Shift+Enter per ticket 41, or loaded back from
    // `$EDITOR` per ticket 42) shows its tail -- where the cursor
    // conceptually is -- once the buffer grows past the Prompt box's single
    // inner row, instead of leaving line 2+ unrendered and invisible. Without
    // this, editing a multi-line buffer in `$EDITOR` produced no visible
    // feedback in the Prompt box at all: `Paragraph` with no scroll always
    // draws starting at line 0, so anything past the first line was silently
    // clipped.
    let prompt_inner_height = chunks[2].height.saturating_sub(2); // top/bottom border
    let prompt_line_count = state.prompt_input.split('\n').count() as u16;
    let prompt_scroll_y = prompt_line_count.saturating_sub(prompt_inner_height);
    let prompt = Paragraph::new(state.prompt_input.as_str())
        .wrap(Wrap { trim: false })
        .scroll((prompt_scroll_y, 0))
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

/// Caps `lines` at [`MAX_DIFF_LINES`], appending a "(N more lines)" marker
/// summarizing the remainder, so a huge write diff can't bury the
/// permission prompt under an arbitrarily long transcript entry.
fn truncate_diff(mut lines: Vec<String>) -> Vec<String> {
    if lines.len() > MAX_DIFF_LINES {
        let remaining = lines.len() - MAX_DIFF_LINES;
        lines.truncate(MAX_DIFF_LINES);
        lines.push(format!("({remaining} more lines)"));
    }
    lines
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

    /// Ticket 42 ($EDITOR integration): temporarily leaves raw mode and the
    /// alternate screen so a suspended `$EDITOR` subprocess can use the
    /// terminal normally, mirroring `restore_terminal`'s best-effort
    /// semantics -- the guard's own `Drop` still restores the terminal on
    /// the way out of `run_blocking` regardless of whether `resume` below
    /// is ever reached.
    fn suspend(&self) {
        restore_terminal();
    }

    /// Re-enters raw mode and the alternate screen after `suspend`, so the
    /// render loop can keep drawing.
    fn resume(&self) -> io::Result<()> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(())
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

/// Errors from an `$EDITOR` invocation (ticket 42, editor-integration).
/// Distinct from [`TuiError`] since these can occur independent of terminal
/// setup -- e.g. in the unit test, which exercises
/// [`edit_buffer_with_editor`] directly without a live terminal.
#[derive(Debug, thiserror::Error)]
enum EditorError {
    /// The resolved editor command was empty (e.g. `$EDITOR` set to
    /// whitespace only).
    #[error("no editor command to run")]
    NotSet,
    /// The editor process could not be spawned at all (e.g. command not
    /// found).
    #[error("failed to spawn editor: {0}")]
    Spawn(#[source] io::Error),
    /// The editor process ran but exited with a non-zero status.
    #[error("editor exited with non-zero status: {0}")]
    NonZeroExit(ExitStatus),
    /// Writing the buffer to, or reading it back from, the temp file
    /// failed.
    #[error("editor temp file io error: {0}")]
    Io(#[source] io::Error),
}

/// Writes `buffer` to a fresh temp file, spawns `editor_command` against
/// that file, waits for it to exit, and on success reads the file back.
///
/// `editor_command` is parsed as a whitespace-separated program plus
/// arguments (the common `$EDITOR="code --wait"` convention), with the temp
/// file's path appended as the final argument. Takes the command as a plain
/// string rather than reading `$EDITOR` itself, so it's unit-testable
/// against a scripted stand-in without mutating process-wide environment
/// state.
///
/// A single trailing `\n`, if present, is trimmed from the file's contents
/// before returning: virtually every text editor appends a final newline on
/// save, and the prompt buffer's existing contract (ticket 41,
/// multiline-input: append-only, no forced trailing newline) shouldn't gain
/// one merely from a round trip through `$EDITOR`.
fn edit_buffer_with_editor(buffer: &str, editor_command: &str) -> Result<String, EditorError> {
    let mut parts = editor_command.split_whitespace();
    let program = parts.next().ok_or(EditorError::NotSet)?;
    let args: Vec<&str> = parts.collect();

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp_path = std::env::temp_dir().join(format!("rokr-editor-{}-{nanos}.txt", std::process::id()));
    std::fs::write(&temp_path, buffer).map_err(EditorError::Io)?;

    let status = match Command::new(program).args(&args).arg(&temp_path).status() {
        Ok(status) => status,
        Err(io_err) => {
            let _ = std::fs::remove_file(&temp_path);
            return Err(EditorError::Spawn(io_err));
        }
    };

    if !status.success() {
        let _ = std::fs::remove_file(&temp_path);
        return Err(EditorError::NonZeroExit(status));
    }

    let mut contents = std::fs::read_to_string(&temp_path).map_err(EditorError::Io)?;
    let _ = std::fs::remove_file(&temp_path);
    if contents.ends_with('\n') {
        contents.pop();
    }
    Ok(contents)
}

/// Handles the editor keybinding (ticket 42, editor-integration): reads
/// `$EDITOR` (falling back to `vi` if unset or empty), suspends the
/// terminal via `guard`, runs [`edit_buffer_with_editor`] against the
/// current prompt buffer, and resumes the terminal -- regardless of whether
/// the editor invocation succeeded, so a spawn failure or non-zero exit
/// can't strand the terminal in a suspended state. On success,
/// `state.prompt_input` is replaced with the edited contents; on failure,
/// the buffer is left untouched and an error is appended to
/// `state.view_lines` for visibility. `terminal.clear()` forces a full
/// repaint on the next draw, since the alternate-screen round trip through
/// the suspended editor leaves ratatui's cached buffer stale.
fn run_editor_keybinding(
    state: &mut AppState,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    guard: &TerminalGuard,
) -> io::Result<()> {
    let editor_command = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());

    guard.suspend();
    let result = edit_buffer_with_editor(&state.prompt_input, &editor_command);
    guard.resume()?;
    terminal.clear()?;

    match result {
        Ok(edited) => state.prompt_input = edited,
        Err(err) => state.view_lines.push(format!("$EDITOR failed: {err}")),
    }
    Ok(())
}

/// Runs the TUI event loop: draws `Header`/`View`/`Prompt`, and exits on
/// `q` (when the prompt is empty) or Ctrl+C (always), restoring the
/// terminal on every exit path. Fails fast with [`TuiError::NotATty`] if
/// stdout isn't a terminal, rather than corrupting non-interactive output.
///
/// `submit` is called with the prompt text and a [`PermissionHandle`]
/// whenever the user presses Enter on a non-empty prompt that does not
/// start with `/`; it is deliberately generic (rather than depending on
/// `rokr-core`/`rokr-provider` types) so this crate stays decoupled from the
/// message model and provider abstraction — the caller (typically `rokr`'s
/// `main.rs`) wires those in, bridging its own permission-request type to
/// [`PermissionRequest`] via the handle. The call runs on a spawned tokio
/// task so the blocking crossterm event loop never stalls waiting on it;
/// [`AppState::pending`] reflects the in-flight state in the meantime, and
/// [`AppState::permission_request`] reflects a gated tool call awaiting the
/// user's decision mid-`submit`.
///
/// `command` is called instead of `submit` (see [`route_input`]) whenever
/// the prompt text starts with `/`; it has no `PermissionHandle` since
/// rokr-tui doesn't know what any given command means — `main.rs` interprets
/// literal command strings like `/compact`. Its resolved `String` is
/// displayed the same way a `submit` reply is.
///
/// `history` (ticket 40, prompt-history) seeds [`AppState::history`] for
/// Up/Down recall, and `on_history_append` is invoked with each submitted
/// prompt so the caller (`main.rs`) can persist it — both are primitives
/// only, since rokr-tui must not depend on rokr-session.
pub async fn run<F, Fut, C, Fut2>(
    submit: F,
    command: C,
    history: Vec<String>,
    on_history_append: impl Fn(String) + Send + Sync + 'static,
) -> Result<(), TuiError>
where
    F: Fn(String, PermissionHandle) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<String, String>> + Send + 'static,
    C: Fn(String) -> Fut2 + Send + Sync + 'static,
    Fut2: Future<Output = String> + Send + 'static,
{
    if !io::stdout().is_terminal() {
        return Err(TuiError::NotATty);
    }

    let handle = tokio::runtime::Handle::current();
    // Ticket 40 (prompt-history): wrapped in `Arc` so it can be cheaply
    // cloned into a fresh `handle.spawn_blocking` call on every Enter-press
    // (see `event_loop`) -- `PromptHistory::append` is blocking fs IO and
    // must never run inline on the crossterm-polling thread (this
    // codebase's "never block the render loop" principle, ADR 0008).
    let on_history_append: Arc<dyn Fn(String) + Send + Sync> = Arc::new(on_history_append);

    tokio::task::spawn_blocking(move || {
        run_blocking(handle, submit, command, history, on_history_append)
    })
    .await
    .unwrap_or_else(|join_err| std::panic::resume_unwind(join_err.into_panic()))
}

fn run_blocking<F, Fut, C, Fut2>(
    handle: tokio::runtime::Handle,
    submit: F,
    command: C,
    history: Vec<String>,
    on_history_append: Arc<dyn Fn(String) + Send + Sync>,
) -> Result<(), TuiError>
where
    F: Fn(String, PermissionHandle) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<String, String>> + Send + 'static,
    C: Fn(String) -> Fut2 + Send + Sync + 'static,
    Fut2: Future<Output = String> + Send + 'static,
{
    install_panic_hook();
    let guard = TerminalGuard::enter()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut state = AppState {
        history,
        ..AppState::default()
    };

    event_loop(&mut terminal, &mut state, &handle, &submit, &command, &on_history_append, &guard)
}

fn event_loop<F, Fut, C, Fut2>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    handle: &tokio::runtime::Handle,
    submit: &F,
    command: &C,
    on_history_append: &Arc<dyn Fn(String) + Send + Sync>,
    guard: &TerminalGuard,
) -> Result<(), TuiError>
where
    F: Fn(String, PermissionHandle) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<String, String>> + Send + 'static,
    C: Fn(String) -> Fut2 + Send + Sync + 'static,
    Fut2: Future<Output = String> + Send + 'static,
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
    // Ticket 40 (prompt-history): `None` = not currently navigating
    // (`prompt_input` is either empty or genuinely user-typed text);
    // `Some(index)` while an Up/Down walk is in progress, indexing into
    // `state.history`.
    let mut history_cursor: Option<usize> = None;

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

                    // Checked before `should_quit` (F-006): while a
                    // permission decision is pending, `q` must deny that
                    // decision rather than fall through to `should_quit`'s
                    // "q quits when the prompt is empty" branch, which
                    // would otherwise always fire here since the prompt
                    // input box isn't being typed into during a permission
                    // decision. Ctrl+C is still a hard quit even here —
                    // `handle_permission_key` returns `Quit` for it.
                    if let Some(responder) = pending_permission_responder.take() {
                        match handle_permission_key(key.code, key.modifiers) {
                            PermissionKeyAction::Quit => return Ok(()),
                            PermissionKeyAction::Allow => {
                                let _ = responder.send(true);
                                state.permission_request = None;
                                dirty = true;
                            }
                            PermissionKeyAction::Deny => {
                                if let Some(request) = &state.permission_request {
                                    state
                                        .view_lines
                                        .push(format!("{}: permission denied", request.tool_name));
                                }
                                let _ = responder.send(false);
                                state.permission_request = None;
                                dirty = true;
                            }
                            PermissionKeyAction::Ignore => {
                                // Not a decision key: put the responder back
                                // and keep waiting, same as ignoring any
                                // other key while `pending` is true below.
                                pending_permission_responder = Some(responder);
                            }
                        }
                        continue;
                    }

                    if should_quit(key.code, key.modifiers, state.prompt_input.is_empty(), state.pending) {
                        return Ok(());
                    }

                    if state.pending {
                        continue;
                    }

                    match key.code {
                        KeyCode::Enter if handle_enter_key(state, key.modifiers) => {
                            // Ticket 41 (multiline-input): Alt+Enter or
                            // Shift+Enter inserted a newline into
                            // `prompt_input` instead of submitting (see
                            // `handle_enter_key`) -- typing, so it exits any
                            // history walk in progress the same way
                            // `Char`/`Backspace` do below, rather than
                            // leaving a stale recall cursor pointed at a
                            // buffer that's now been hand-edited.
                            history_cursor = None;
                            dirty = true;
                        }
                        KeyCode::Enter if !state.prompt_input.is_empty() => {
                            let input = std::mem::take(&mut state.prompt_input);
                            // Ticket 40 (prompt-history): captured at the
                            // moment of submission, independent of whether
                            // the outgoing call (spawned below) ever
                            // succeeds -- a prompt should be recallable the
                            // instant it's sent, not gated on a network
                            // round-trip. Appended to the in-memory list
                            // immediately (so Up/Down sees it this run
                            // without waiting on disk IO) and to the
                            // on-disk history file via a spawned blocking
                            // task (fs IO must never run inline on this
                            // thread).
                            history_cursor = None;
                            state.history.push(input.clone());
                            {
                                let cb = on_history_append.clone();
                                let entry = input.clone();
                                handle.spawn_blocking(move || cb(entry));
                            }
                            state.view_lines.push(format!("> {input}"));
                            state.pending = true;
                            dirty = true;

                            match route_input(input) {
                                InputRoute::Submit(input) => {
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
                                InputRoute::Command(input) => {
                                    let command_fut = command(input);
                                    let tx = tx.clone();
                                    handle.spawn(async move {
                                        let outcome = command_fut.await;
                                        let _ = tx.send(Ok(outcome));
                                    });
                                }
                            }
                        }
                        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            // Ticket 42 (editor-integration): like
                            // Enter/Char/Backspace above, exits any history
                            // walk in progress since the buffer is about to
                            // be replaced wholesale by whatever the editor
                            // saves.
                            history_cursor = None;
                            run_editor_keybinding(state, terminal, guard)?;
                            dirty = true;
                        }
                        KeyCode::Char(c) => {
                            // Ticket 40: typing breaks a history walk in
                            // progress (the buffer is now genuinely
                            // user-edited, not a pure recall), but keeps
                            // whatever text is currently there.
                            history_cursor = None;
                            state.prompt_input.push(c);
                            dirty = true;
                        }
                        KeyCode::Backspace => {
                            history_cursor = None;
                            state.prompt_input.pop();
                            dirty = true;
                        }
                        KeyCode::Up => {
                            if let Some((index, text)) = history_navigate_up(
                                &state.history,
                                history_cursor,
                                state.prompt_input.is_empty(),
                            ) {
                                history_cursor = Some(index);
                                state.prompt_input = text;
                                dirty = true;
                            }
                        }
                        KeyCode::Down => {
                            match history_navigate_down(&state.history, history_cursor) {
                                HistoryDown::Recall { index, text } => {
                                    history_cursor = Some(index);
                                    state.prompt_input = text;
                                    dirty = true;
                                }
                                HistoryDown::ClearToEmpty => {
                                    history_cursor = None;
                                    state.prompt_input.clear();
                                    dirty = true;
                                }
                                HistoryDown::NoOp => {}
                            }
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

/// `q` quits only when the prompt is empty AND no submit/command call is
/// still in flight, so it doesn't clash with typing a prompt that uses the
/// letter. Without the `!pending` guard, a `q` keystroke arriving while
/// `pending` is true would still see an empty prompt — keystrokes typed
/// during a pending call are dropped rather than accumulated into
/// `prompt_input` (see `event_loop`) — and would quit the whole app out
/// from under a user who was mid-typing their next message, silently
/// discarding it. Ctrl+C always quits as a hard escape hatch regardless of
/// prompt content or pending state.
fn should_quit(code: KeyCode, modifiers: KeyModifiers, prompt_is_empty: bool, pending: bool) -> bool {
    (matches!(code, KeyCode::Char('q')) && prompt_is_empty && !pending)
        || (matches!(code, KeyCode::Char('c')) && modifiers.contains(KeyModifiers::CONTROL))
}

/// Ticket 41 (multiline-input): an `Enter` keypress submits the prompt
/// UNLESS `modifiers` carries ALT or SHIFT, in which case it instead
/// appends a newline to `state.prompt_input` and returns `true` so the
/// caller (`event_loop`'s `KeyCode::Enter` match guard) knows to treat this
/// as a buffer edit rather than falling through to the submit arm.
///
/// PRD decision 5 (phase-5-session-management) fixes this split for the
/// phase -- Enter always submits, Alt+Enter/Shift+Enter always inserts,
/// not configurable -- so no further key-binding lookup is needed here.
///
/// SHIFT is checked alongside ALT for terminals that do transmit it (some
/// modern terminal + keyboard-protocol combinations do), even though this
/// crate doesn't opt into crossterm's kitty keyboard-enhancement flags, so
/// a raw xterm-style PTY can only ever be driven via ALT in practice (see
/// the crossterm-decoding doc comment on
/// `multiline_prompt_composed_with_shift_enter_submits_as_single_prompt_on_enter`
/// in `crates/rokr/tests/tui_test.rs` for the byte-level detail).
///
/// Deliberately append-only (no cursor-position tracking): this crate's
/// input buffer has no separate edit-cursor concept yet (`Char`/
/// `Backspace` only ever act at the end of `prompt_input`), and the
/// acceptance criterion only requires composing and submitting a
/// multi-line prompt, not mid-buffer cursor movement -- adding real
/// cursor-aware insertion would be building more editor than this ticket
/// asks for.
fn handle_enter_key(state: &mut AppState, modifiers: KeyModifiers) -> bool {
    if modifiers.contains(KeyModifiers::ALT) || modifiers.contains(KeyModifiers::SHIFT) {
        state.prompt_input.push('\n');
        true
    } else {
        false
    }
}

/// What a keypress means while a permission prompt is showing (i.e.
/// `pending_permission_responder` is `Some` in [`event_loop`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionKeyAction {
    /// `y`: run the gated tool call.
    Allow,
    /// `n`, `q`, or `Esc`: deny the gated tool call. `q`/`Esc` intentionally
    /// do *not* fall through to whole-app quit here (see F-006) — a
    /// permission prompt already blocks all other input, so "back out of
    /// this decision" reads more naturally as deny than as exiting the
    /// entire session.
    Deny,
    /// Ctrl+C: hard-quit escape hatch that survives even a pending
    /// permission decision.
    Quit,
    /// Any other key: not a decision, keep waiting.
    Ignore,
}

/// Decides the [`PermissionKeyAction`] for a keypress received while a
/// permission prompt is showing. Extracted as a pure function so it's
/// unit-testable without driving a live `Terminal`/event loop.
fn handle_permission_key(code: KeyCode, modifiers: KeyModifiers) -> PermissionKeyAction {
    if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
        return PermissionKeyAction::Quit;
    }
    match code {
        KeyCode::Char('y') => PermissionKeyAction::Allow,
        KeyCode::Char('n') | KeyCode::Char('q') | KeyCode::Esc => PermissionKeyAction::Deny,
        _ => PermissionKeyAction::Ignore,
    }
}

/// How a line of submitted prompt input should be routed on Enter:
/// slash-prefixed input goes to the `command` callback (rokr-tui stays
/// unaware of what any specific command means — `main.rs` interprets
/// `/compact` etc.), everything else goes through the normal `submit` path
/// unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InputRoute {
    Command(String),
    Submit(String),
}

/// Classifies `input` for [`InputRoute`]. Extracted as a pure function so
/// it's unit-testable without driving a live `Terminal`/event loop, mirroring
/// `should_quit`/`handle_permission_key`.
fn route_input(input: String) -> InputRoute {
    if input.starts_with('/') {
        InputRoute::Command(input)
    } else {
        InputRoute::Submit(input)
    }
}

/// Ticket 40 (prompt-history): decides the new `(index, text)` when Up is
/// pressed, or `None` for a no-op. Pure and side-effect-free so it's
/// unit-testable without a live event loop, mirroring `should_quit`/
/// `handle_permission_key`/`route_input`.
///
/// Behavior (shell-like, PRD decision 5's "prev/next recall"):
/// - Up on an empty prompt, when not already navigating, starts a walk at
///   the most recent entry.
/// - Up on a NON-empty prompt that is genuinely typed (not itself a history
///   recall -- i.e. `cursor` is `None`) is a no-op: recall only starts from
///   an empty buffer, so a half-typed prompt is never silently clobbered.
///   This is the "only recall from empty buffer" choice (as opposed to
///   "replace any typed content"), picked because it can never destroy
///   unsent text the user was actively composing.
/// - Once already navigating (`cursor` is `Some`), further Up presses walk
///   to the next-older entry regardless of the buffer's current content;
///   at the oldest entry, Up is a no-op (stays put).
fn history_navigate_up(
    history: &[String],
    cursor: Option<usize>,
    prompt_is_empty: bool,
) -> Option<(usize, String)> {
    match cursor {
        None if prompt_is_empty && !history.is_empty() => {
            let index = history.len() - 1;
            Some((index, history[index].clone()))
        }
        None => None,
        Some(0) => None,
        Some(index) => {
            let index = index - 1;
            Some((index, history[index].clone()))
        }
    }
}

/// Result of a Down navigation attempt (see [`history_navigate_down`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum HistoryDown {
    /// Not currently navigating -- Down does nothing.
    NoOp,
    /// Walk to a newer entry: set the cursor to `index`, buffer to `text`.
    Recall { index: usize, text: String },
    /// Walking past the newest entry exits the walk entirely: cursor back
    /// to `None`, buffer back to empty -- shell-like "keep going forward
    /// and land back on a blank line".
    ClearToEmpty,
}

/// Ticket 40 (prompt-history): mirrors [`history_navigate_up`] for Down.
/// Pure and side-effect-free for the same testability reasons.
fn history_navigate_down(history: &[String], cursor: Option<usize>) -> HistoryDown {
    match cursor {
        None => HistoryDown::NoOp,
        Some(index) if index + 1 < history.len() => {
            let index = index + 1;
            HistoryDown::Recall {
                index,
                text: history[index].clone(),
            }
        }
        Some(_) => HistoryDown::ClearToEmpty,
    }
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

    /// F-004: a long transcript pushing the permission prompt line past the
    /// bottom of the View pane must not hide it — `draw` should scroll so
    /// "[y] allow  [n] deny" stays on screen.
    #[test]
    fn permission_prompt_stays_visible_with_long_transcript() {
        // 80 columns wide (only the height is constrained, per the ticket)
        // so none of the short lines below wrap — that isolates the
        // scroll-offset behavior under test from the unwrapped-line-count
        // approximation's known limitation (documented on `draw`), which is
        // a separate concern from F-004's bottom-anchoring fix.
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = AppState {
            view_lines: (0..30).map(|i| format!("line {i}")).collect(),
            permission_request: Some(PermissionRequest {
                tool_name: "bash".to_string(),
                detail: PermissionDetail::Text("some long command".to_string()),
            }),
            ..Default::default()
        };

        terminal.draw(|frame| draw(frame, &state)).unwrap();

        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let prompt_visible = (area.y..area.y + area.height).any(|y| {
            let row = row_text(buffer, y);
            row.contains("[y] allow") && row.contains("[n] deny")
        });
        assert!(
            prompt_visible,
            "expected the '[y] allow  [n] deny' line to be visible somewhere in the rendered buffer"
        );
    }

    /// F-010: a single `view_lines` entry containing many embedded `\n`
    /// characters (as `event_loop` pushes whole model responses as one
    /// entry) must still be counted toward the bottom-anchor scroll offset
    /// on a per-rendered-line basis, not as a single Vec entry — otherwise
    /// the scroll computation undercounts and the permission prompt gets
    /// pushed off the bottom of the View pane, same symptom F-004 fixed.
    #[test]
    fn permission_prompt_stays_visible_with_embedded_newlines_in_single_entry() {
        // 80 columns wide so none of the short lines below wrap, isolating
        // the embedded-newline counting behavior under test from the
        // separate wrap-approximation limitation documented on `draw`.
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let single_entry = (0..30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let state = AppState {
            view_lines: vec![single_entry],
            permission_request: Some(PermissionRequest {
                tool_name: "bash".to_string(),
                detail: PermissionDetail::Text("some long command".to_string()),
            }),
            ..Default::default()
        };

        terminal.draw(|frame| draw(frame, &state)).unwrap();

        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let prompt_visible = (area.y..area.y + area.height).any(|y| {
            let row = row_text(buffer, y);
            row.contains("[y] allow") && row.contains("[n] deny")
        });
        assert!(
            prompt_visible,
            "expected the '[y] allow  [n] deny' line to be visible somewhere in the rendered buffer"
        );
    }

    #[test]
    fn truncate_diff_leaves_short_diffs_untouched() {
        let lines: Vec<String> = (0..5).map(|i| format!("-{i}")).collect();
        let result = truncate_diff(lines.clone());
        assert_eq!(result, lines);
    }

    #[test]
    fn truncate_diff_caps_long_diffs_with_marker() {
        let lines: Vec<String> = (0..25).map(|i| format!("-{i}")).collect();
        let result = truncate_diff(lines);
        assert_eq!(result.len(), MAX_DIFF_LINES + 1);
        assert_eq!(result[MAX_DIFF_LINES], "(7 more lines)");
    }

    #[test]
    fn handle_permission_key_allow_on_y() {
        assert_eq!(
            handle_permission_key(KeyCode::Char('y'), KeyModifiers::NONE),
            PermissionKeyAction::Allow
        );
    }

    #[test]
    fn handle_permission_key_deny_on_n() {
        assert_eq!(
            handle_permission_key(KeyCode::Char('n'), KeyModifiers::NONE),
            PermissionKeyAction::Deny
        );
    }

    /// F-006: `q` while a permission prompt is pending must deny the
    /// decision, not fall through to whole-app quit.
    #[test]
    fn handle_permission_key_deny_on_q() {
        assert_eq!(
            handle_permission_key(KeyCode::Char('q'), KeyModifiers::NONE),
            PermissionKeyAction::Deny
        );
    }

    #[test]
    fn handle_permission_key_deny_on_esc() {
        assert_eq!(
            handle_permission_key(KeyCode::Esc, KeyModifiers::NONE),
            PermissionKeyAction::Deny
        );
    }

    /// F-006: Ctrl+C must stay a hard quit even during a pending permission
    /// decision.
    #[test]
    fn handle_permission_key_quit_on_ctrl_c() {
        assert_eq!(
            handle_permission_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            PermissionKeyAction::Quit
        );
    }

    #[test]
    fn handle_permission_key_ignores_other_keys() {
        assert_eq!(
            handle_permission_key(KeyCode::Char('x'), KeyModifiers::NONE),
            PermissionKeyAction::Ignore
        );
    }

    #[test]
    fn should_quit_on_bare_q_when_prompt_empty_and_not_pending() {
        assert!(should_quit(KeyCode::Char('q'), KeyModifiers::NONE, true, false));
    }

    /// Regression test for the bug fixed here: a `q` keystroke arriving while a
    /// submit/command call is still pending must not quit the app. Keystrokes
    /// typed during `pending` are dropped without being added to
    /// `prompt_input` (see `event_loop`), so `prompt_is_empty` is always true
    /// throughout the pending window — without the `!pending` guard, any `q`
    /// anywhere in a user's next message (e.g. the "q" in "any question?")
    /// would silently quit the whole app before the message could be
    /// submitted.
    #[test]
    fn should_quit_does_not_fire_on_bare_q_while_pending() {
        assert!(!should_quit(KeyCode::Char('q'), KeyModifiers::NONE, true, true));
    }

    #[test]
    fn should_quit_on_ctrl_c_even_while_pending() {
        assert!(should_quit(KeyCode::Char('c'), KeyModifiers::CONTROL, true, true));
    }

    #[test]
    fn should_quit_does_not_fire_on_q_when_prompt_not_empty() {
        assert!(!should_quit(KeyCode::Char('q'), KeyModifiers::NONE, false, false));
    }

    #[test]
    fn input_starting_with_slash_is_routed_to_command_handler_not_normal_submit() {
        assert_eq!(
            route_input("/compact".to_string()),
            InputRoute::Command("/compact".to_string())
        );
    }

    #[test]
    fn history_navigate_up_starts_walk_from_empty_prompt_at_most_recent_entry() {
        let history = vec!["first".to_string(), "second".to_string(), "third".to_string()];
        let result = history_navigate_up(&history, None, true);
        assert_eq!(result, Some((2, "third".to_string())));
    }

    #[test]
    fn history_navigate_up_is_noop_when_prompt_not_empty_and_not_already_navigating() {
        let history = vec!["only".to_string()];
        let result = history_navigate_up(&history, None, false);
        assert_eq!(result, None);
    }

    #[test]
    fn history_navigate_up_walks_to_older_entry_when_already_navigating() {
        let history = vec!["first".to_string(), "second".to_string(), "third".to_string()];
        let result = history_navigate_up(&history, Some(2), false);
        assert_eq!(result, Some((1, "second".to_string())));
    }

    #[test]
    fn history_navigate_up_is_noop_at_oldest_entry() {
        let history = vec!["first".to_string(), "second".to_string()];
        let result = history_navigate_up(&history, Some(0), false);
        assert_eq!(result, None);
    }

    #[test]
    fn history_navigate_down_is_noop_when_not_navigating() {
        let history = vec!["first".to_string()];
        assert_eq!(history_navigate_down(&history, None), HistoryDown::NoOp);
    }

    #[test]
    fn history_navigate_down_walks_to_newer_entry() {
        let history = vec!["first".to_string(), "second".to_string(), "third".to_string()];
        assert_eq!(
            history_navigate_down(&history, Some(0)),
            HistoryDown::Recall { index: 1, text: "second".to_string() }
        );
    }

    #[test]
    fn history_navigate_down_clears_to_empty_past_newest_entry() {
        let history = vec!["first".to_string(), "second".to_string()];
        assert_eq!(history_navigate_down(&history, Some(1)), HistoryDown::ClearToEmpty);
    }

    /// Ticket 41 (multiline-input) acceptance-adjacent unit test: Shift+Enter
    /// (and Alt+Enter, its raw-PTY-portable equivalent -- see the
    /// crossterm-decoding note on `handle_enter_key`) must insert a newline
    /// into `prompt_input` rather than submitting -- asserted here by
    /// showing the buffer keeps growing (never cleared the way the event
    /// loop's `mem::take` on a real submit would empty it).
    #[test]
    fn shift_enter_inserts_newline_without_submitting() {
        let mut state = AppState {
            prompt_input: "line one".to_string(),
            ..Default::default()
        };

        let inserted = handle_enter_key(&mut state, KeyModifiers::SHIFT);

        assert!(inserted, "expected Shift+Enter to insert a newline, not submit");
        assert_eq!(
            state.prompt_input, "line one\n",
            "expected the newline to be appended and the buffer preserved rather than cleared/submitted"
        );

        // Alt+Enter must behave identically (both are treated as "insert a
        // newline" per the ticket).
        let inserted_alt = handle_enter_key(&mut state, KeyModifiers::ALT);
        assert!(inserted_alt, "expected Alt+Enter to insert a newline, not submit");
        assert_eq!(state.prompt_input, "line one\n\n");

        // A plain Enter (no ALT/SHIFT) must NOT be treated as a newline
        // insert -- that's the event loop's cue to submit instead.
        let inserted_plain = handle_enter_key(&mut state, KeyModifiers::NONE);
        assert!(!inserted_plain, "expected plain Enter not to insert a newline");
        assert_eq!(state.prompt_input, "line one\n\n", "plain Enter must not modify the buffer");
    }

    /// Ticket 42 (editor-integration) unit test: `edit_buffer_with_editor`
    /// writes the given buffer to a temp file, runs the given (scripted, for
    /// this test) editor command against it, and reads the edited contents
    /// back once the process exits -- exercised here with a tiny
    /// non-interactive shell script standing in for a real interactive
    /// `$EDITOR`, since this unit test (unlike the PTY acceptance test in
    /// `crates/rokr/tests/tui_test.rs`) has no live terminal to
    /// suspend/resume.
    #[test]
    fn editor_suspend_writes_buffer_to_temp_file_and_reloads_edited_contents_on_exit() {
        use std::os::unix::fs::PermissionsExt;

        let script_dir = std::env::temp_dir().join(format!(
            "rokr-tui-editor-unit-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&script_dir).expect("failed to create script dir");
        let script_path = script_dir.join("fake_editor.sh");
        std::fs::write(&script_path, "#!/bin/sh\nprintf '\\nedited by script\\n' >> \"$1\"\n")
            .expect("failed to write fake editor script");
        let mut perms = std::fs::metadata(&script_path)
            .expect("failed to stat fake editor script")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms)
            .expect("failed to make fake editor script executable");

        let result = edit_buffer_with_editor(
            "original line",
            script_path.to_str().expect("script path should be valid utf-8"),
        )
        .expect("scripted editor should succeed");

        assert_eq!(
            result, "original line\nedited by script",
            "expected the buffer written to the temp file, plus the scripted editor's \
             appended line, read back with the trailing newline trimmed"
        );

        let _ = std::fs::remove_dir_all(&script_dir);
    }
}
