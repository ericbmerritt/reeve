//! TUI event loop for the Reeve lead chat screen.
//!
//! [`run`] owns terminal lifecycle (raw mode, alternate screen) and drives the
//! main event loop. The loop reacts to two event sources:
//!
//! 1. **Filesystem watcher events** — reloads agent state from disk and
//!    triggers a redraw. Debounced to 250 ms by [`crate::watcher`].
//! 2. **Crossterm keyboard events** — updates the input buffer, submits
//!    messages, or exits.
//!
//! The TUI talks to the runtime only through the filesystem. No sockets, no RPC.
//!
//! # Terminal cleanup
//!
//! Raw mode and alternate screen are restored on exit regardless of how `run`
//! returns (normal exit or error), via an RAII-style cleanup guard.

use std::io::{self, Stdout};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reeve_runtime::{IdentityRegistry, OperatorKeyStore};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEvent,
    MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, ExecutableCommand as _};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use reeve_runtime::AgentDirs;

use crate::panopticon::read_snapshot as read_panopticon_snapshot;
use crate::reader::{read_conversation, read_cost, read_status};
use crate::state::{AppState, Screen};
use crate::submit::submit_message;
use crate::watcher::watch_agent_dir;

/// Poll timeout for crossterm events. The loop also reacts to watcher signals,
/// so this just sets the maximum latency between a watcher event and a redraw.
const POLL_TIMEOUT: Duration = Duration::from_millis(100);

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors surfaced by the TUI event loop.
#[derive(Debug)]
pub enum TuiError {
    /// Terminal setup, draw, or restore failed.
    Terminal(io::Error),
    /// Filesystem watcher could not be started.
    Watcher(notify::Error),
    /// A message submission attempt failed.
    Submit(crate::submit::SubmitError),
}

impl std::fmt::Display for TuiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Terminal(err) => write!(f, "terminal error: {err}"),
            Self::Watcher(err) => write!(f, "filesystem watcher error: {err}"),
            Self::Submit(err) => write!(f, "message submit error: {err}"),
        }
    }
}

impl std::error::Error for TuiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Terminal(err) => Some(err),
            Self::Watcher(err) => Some(err),
            Self::Submit(err) => Some(err),
        }
    }
}

// ── Terminal RAII guard ───────────────────────────────────────────────────────

/// RAII guard that restores the terminal on drop.
///
/// Constructed by [`setup_terminal`]; dropped at the end of [`run`] or on
/// early error return. Raw mode and alternate screen are always restored.
///
/// No stdout is stored here: `CrosstermBackend` already owns one handle, and
/// storing a second long-lived handle would give two independent `io::Stdout`
/// values writing to fd 1. Instead, `drop` creates an ephemeral handle only at
/// cleanup time, after the event loop (and the backend) have exited.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Ignore errors during cleanup: nothing useful can be done if restore fails.
        let _ = execute!(io::stdout(), DisableMouseCapture);
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Enable raw mode and alternate screen, returning a guard that restores them.
fn setup_terminal() -> Result<(Terminal<CrosstermBackend<Stdout>>, TerminalGuard), TuiError> {
    enable_raw_mode().map_err(TuiError::Terminal)?;
    // Guard is constructed immediately after raw mode is enabled so that, if
    // EnterAlternateScreen fails, drop() will still call disable_raw_mode().
    // The LeaveAlternateScreen in drop() is harmless when the alternate screen
    // was never entered.
    let guard = TerminalGuard;
    let mut stdout = io::stdout();
    stdout
        .execute(EnterAlternateScreen)
        .map_err(TuiError::Terminal)?;
    // Capture mouse so wheel events route into the event loop for
    // scrolling the conversation pane. DisableMouseCapture runs in
    // TerminalGuard::drop so the terminal is restored even on panic.
    stdout
        .execute(EnableMouseCapture)
        .map_err(TuiError::Terminal)?;
    let backend = CrosstermBackend::new(io::stdout());
    let terminal = Terminal::new(backend).map_err(TuiError::Terminal)?;
    Ok((terminal, guard))
}

/// Pick the first operator-typed identity from the registry. Used at startup
/// to seed `AppState.operator_id` so the renderer can label inbound entries
/// signed by the operator as `"you"`. Returns `None` if no operator exists or
/// the registry read fails — the renderer falls back to a short-id label.
fn first_operator_id(registry: &IdentityRegistry) -> Option<reeve_types::IdentityId> {
    registry.list().ok()?.into_iter().find_map(|stored| {
        (stored.identity().identity_type == reeve_types::IdentityType::Operator)
            .then(|| stored.identity().identity_id)
    })
}

// ── State loading ─────────────────────────────────────────────────────────────

/// Reload all agent state from disk into `state`.
///
/// Called on every watcher callback. Individual reads return safe defaults on
/// any filesystem error (see `crate::reader`).
///
/// `data_dir` and `agent_registry_path` drive the panopticon snapshot read.
/// The lead-chat reload and the panopticon refresh share a trigger right
/// now: any change in the lead's directory re-renders both screens. A
/// future commit can extend the watcher to cover every registered agent's
/// state dir so a worker's transition refreshes the panopticon without
/// waiting on the lead.
fn reload_state(
    state: &mut AppState,
    dirs: &AgentDirs,
    data_dir: &Path,
    agent_registry_path: &Path,
) {
    state.status = read_status(&dirs.status_path());
    state.conversation = read_conversation(&dirs.conversation_path());
    state.cost_usd = read_cost(&dirs.cost_path());
    state.panopticon = read_panopticon_snapshot(data_dir, agent_registry_path, state.operator_id);
}

// ── Event loop ────────────────────────────────────────────────────────────────

/// Run the TUI until the user quits.
///
/// Blocks until `q` / `Esc` is pressed or an unrecoverable error occurs.
/// Terminal is restored (raw mode off, alternate screen left) on all exit paths.
///
/// # Parameters
///
/// - `dirs`: the lead agent's filesystem layout; used for status / cost / conversation reads and inbox writes.
/// - `registry`: the identity registry, used by [`submit_message`] to locate the operator identity.
/// - `keystore`: the platform keystore, used by [`submit_message`] to retrieve the operator signing key.
///
/// # Errors
///
/// Returns [`TuiError::Terminal`] on terminal I/O failure,
/// [`TuiError::Watcher`] if the filesystem watcher cannot start, or
/// [`TuiError::Submit`] if a message write fails (not currently surfaced to the
/// user — future iterations should show an inline error).
pub fn run(
    dirs: &AgentDirs,
    data_dir: &Path,
    agent_registry_path: &Path,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<(), TuiError> {
    let (mut terminal, _guard) = setup_terminal()?;

    let needs_reload = Arc::new(AtomicBool::new(true)); // true = load immediately on start
    let needs_reload_clone = Arc::clone(&needs_reload);

    // Kept alive until run() returns.
    let _watcher = watch_agent_dir(dirs.root(), move || {
        needs_reload_clone.store(true, Ordering::Release);
    })
    .map_err(TuiError::Watcher)?;

    let mut state = AppState::default();
    // Resolve the operator identity once at startup so inbound entries can
    // render with "you" for the operator and a distinct sender label for
    // worker/peer replies. If lookup fails the label falls back to a short
    // id, which is still better than the pre-attribution "you for everyone".
    state.operator_id = first_operator_id(registry);

    loop {
        if needs_reload.swap(false, Ordering::Acquire) {
            reload_state(&mut state, dirs, data_dir, agent_registry_path);
        }

        terminal
            .draw(|frame| match state.screen {
                Screen::Chat => crate::ui::draw(frame, &state),
                Screen::Panopticon => {
                    crate::ui_panopticon::draw(frame, &state.panopticon, state.panopticon_focus);
                }
            })
            .map_err(TuiError::Terminal)?;

        // Short timeout keeps watcher latency bounded.
        if event::poll(POLL_TIMEOUT).map_err(TuiError::Terminal)? {
            match event::read().map_err(TuiError::Terminal)? {
                Event::Key(key) => {
                    let prev_screen = state.screen;
                    if handle_key(key, &mut state, dirs, registry, keystore)? {
                        return Ok(());
                    }
                    // Force a fresh panopticon read only on the *transition*
                    // into the panopticon screen, so the operator's first
                    // frame shows current state. Refreshing on every
                    // keystroke while already in the panopticon walks every
                    // agent's status / cost / conversation files per j/k
                    // press — pointless IO at typing cadence.
                    if state.screen == Screen::Panopticon && prev_screen != Screen::Panopticon {
                        state.panopticon = read_panopticon_snapshot(
                            data_dir,
                            agent_registry_path,
                            state.operator_id,
                        );
                    }
                }
                Event::Mouse(mouse) => handle_mouse(mouse, &mut state),
                // Resize triggers a full redraw on the next iteration naturally.
                Event::Resize(_, _) | Event::FocusGained | Event::FocusLost | Event::Paste(_) => {}
            }
        }
    }
}

/// Mouse wheel maps to conversation-pane scroll. The wheel typically emits
/// one event per detent on macOS / Linux; bumping three rows per detent
/// matches the cadence of every other terminal app the operator is used to.
fn handle_mouse(event: MouseEvent, state: &mut AppState) {
    const ROWS_PER_DETENT: u16 = 3;
    match event.kind {
        MouseEventKind::ScrollUp => state.scroll_up(ROWS_PER_DETENT),
        MouseEventKind::ScrollDown => state.scroll_down(ROWS_PER_DETENT),
        MouseEventKind::Down(_)
        | MouseEventKind::Up(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::Moved
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => {}
    }
}

/// Handle one keyboard event. Returns `true` when the TUI should exit.
///
/// Dispatch is screen-aware: chat keys (input typing, scrolling, submit)
/// only fire on [`Screen::Chat`]; panopticon keys (`j`/`k` navigate,
/// `Enter` open focused agent) only fire on [`Screen::Panopticon`].
///
/// `q` (no modifier) quits from either screen; `Tab` toggles. `Esc` is
/// screen-aware: from chat it quits (consistent with prior behavior); from
/// the panopticon it pops back to chat — the vim / k9s / lazygit
/// convention of "Esc = back" that the operator brings with them. Without
/// the screen-aware behavior, an operator who hits Esc expecting to leave
/// the panopticon exits the whole TUI instead.
fn handle_key(
    key: event::KeyEvent,
    state: &mut AppState,
    dirs: &AgentDirs,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<bool, TuiError> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), KeyModifiers::NONE) => return Ok(true),
        (KeyCode::Tab, _) => {
            state.toggle_screen();
            return Ok(false);
        }
        (KeyCode::Esc, _) => match state.screen {
            Screen::Chat => return Ok(true),
            Screen::Panopticon => {
                state.screen = Screen::Chat;
                return Ok(false);
            }
        },
        _ => {}
    }

    match state.screen {
        Screen::Chat => handle_key_chat(key, state, dirs, registry, keystore),
        Screen::Panopticon => Ok(handle_key_panopticon(key, state)),
    }
}

/// Chat-screen key bindings: typing, scrolling, submit on Enter.
fn handle_key_chat(
    key: event::KeyEvent,
    state: &mut AppState,
    dirs: &AgentDirs,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<bool, TuiError> {
    // Conversation-pane scroll. ROWS_PER_PAGE is intentionally a constant
    // rather than terminal height so the operator's muscle memory ('PgUp
    // moves about a screenful') doesn't change with window size.
    const ROWS_PER_PAGE: u16 = 10;
    const ROWS_PER_LINE_NUDGE: u16 = 1;

    match (key.code, key.modifiers) {
        (KeyCode::Enter, KeyModifiers::NONE) => {
            submit_input(state, dirs, registry, keystore)?;
        }

        (KeyCode::Backspace, _) => {
            let mut s = state.input.clone();
            if !s.is_empty() {
                // Remove the last UTF-8 character (not the last byte).
                let trim_pos = s.char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
                s.truncate(trim_pos);
            }
            state.set_input(s);
        }

        (KeyCode::PageUp, _) => state.scroll_up(ROWS_PER_PAGE),
        (KeyCode::PageDown, _) => state.scroll_down(ROWS_PER_PAGE),
        // Shift+Up/Down: fine-grained nudges. Plain arrow keys are reserved
        // for future input-cursor movement so we don't claim them here.
        (KeyCode::Up, KeyModifiers::SHIFT) => state.scroll_up(ROWS_PER_LINE_NUDGE),
        (KeyCode::Down, KeyModifiers::SHIFT) => state.scroll_down(ROWS_PER_LINE_NUDGE),
        (KeyCode::End, _) => state.scroll_to_bottom(),

        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            let mut s = state.input.clone();
            s.push(c);
            state.set_input(s);
        }

        _ => {}
    }

    Ok(false)
}

/// Panopticon-screen key bindings. `j`/`k` (and arrow keys) navigate the
/// agent table; `Enter` switches back to the chat for now — Phase 6 only
/// hosts the lead's chat, so Enter on a non-lead row currently closes the
/// panopticon without opening a different chat. Phase 7 wires per-agent
/// chat.
fn handle_key_panopticon(key: event::KeyEvent, state: &mut AppState) -> bool {
    match (key.code, key.modifiers) {
        (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => {
            state.panopticon_focus_down();
        }
        (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => {
            state.panopticon_focus_up();
        }
        // Single-chat universe (Phase 6): close the panopticon and return
        // to the existing chat regardless of which row was focused. Phase
        // 7 will branch on the focused agent here.
        (KeyCode::Enter, _) => state.screen = Screen::Chat,
        _ => {}
    }
    false
}

/// Submit the current input buffer and clear it.
///
/// If the buffer is empty or all-whitespace, does nothing.
/// On submission error, the error propagates to the caller; future iterations
/// could catch it and display an inline error message.
fn submit_input(
    state: &mut AppState,
    dirs: &AgentDirs,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<(), TuiError> {
    let payload = state.input.trim().to_owned();
    if payload.is_empty() {
        return Ok(());
    }
    submit_message(&payload, dirs, registry, keystore).map_err(TuiError::Submit)?;
    state.set_input(String::new());
    // Sending a message means the operator wants to see the reply; snap the
    // view back to the bottom so the response auto-scrolls into focus.
    state.scroll_to_bottom();
    Ok(())
}
