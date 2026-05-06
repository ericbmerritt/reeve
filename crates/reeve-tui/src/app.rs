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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, ExecutableCommand as _};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use reeve_runtime::AgentDirs;

use crate::reader::{read_conversation, read_cost, read_status};
use crate::state::AppState;
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
    let backend = CrosstermBackend::new(io::stdout());
    let terminal = Terminal::new(backend).map_err(TuiError::Terminal)?;
    Ok((terminal, guard))
}

// ── State loading ─────────────────────────────────────────────────────────────

/// Reload all agent state from disk into `state`.
///
/// Called on every watcher callback. Individual reads return safe defaults on
/// any filesystem error (see `crate::reader`).
fn reload_state(state: &mut AppState, dirs: &AgentDirs) {
    state.status = read_status(&dirs.status_path());
    state.conversation = read_conversation(&dirs.conversation_path());
    state.cost_usd = read_cost(&dirs.cost_path());
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
///
/// # Errors
///
/// Returns [`TuiError::Terminal`] on terminal I/O failure,
/// [`TuiError::Watcher`] if the filesystem watcher cannot start, or
/// [`TuiError::Submit`] if a message write fails (not currently surfaced to the
/// user — future iterations should show an inline error).
pub fn run(dirs: &AgentDirs) -> Result<(), TuiError> {
    let (mut terminal, _guard) = setup_terminal()?;

    let needs_reload = Arc::new(AtomicBool::new(true)); // true = load immediately on start
    let needs_reload_clone = Arc::clone(&needs_reload);

    // Kept alive until run() returns.
    let _watcher = watch_agent_dir(dirs.root(), move || {
        needs_reload_clone.store(true, Ordering::Release);
    })
    .map_err(TuiError::Watcher)?;

    let mut state = AppState::default();

    loop {
        if needs_reload.swap(false, Ordering::Acquire) {
            reload_state(&mut state, dirs);
        }

        terminal
            .draw(|frame| crate::ui::draw(frame, &state))
            .map_err(TuiError::Terminal)?;

        // Short timeout keeps watcher latency bounded.
        if event::poll(POLL_TIMEOUT).map_err(TuiError::Terminal)? {
            match event::read().map_err(TuiError::Terminal)? {
                Event::Key(key) => {
                    if handle_key(key, &mut state, dirs)? {
                        return Ok(());
                    }
                }
                // Resize triggers a full redraw on the next iteration naturally.
                Event::Resize(_, _)
                | Event::FocusGained
                | Event::FocusLost
                | Event::Mouse(_)
                | Event::Paste(_) => {}
            }
        }
    }
}

/// Handle one keyboard event. Returns `true` when the TUI should exit.
fn handle_key(
    key: event::KeyEvent,
    state: &mut AppState,
    dirs: &AgentDirs,
) -> Result<bool, TuiError> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), KeyModifiers::NONE) | (KeyCode::Esc, _) => {
            return Ok(true);
        }

        (KeyCode::Enter, KeyModifiers::NONE) => {
            submit_input(state, dirs)?;
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

        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            let mut s = state.input.clone();
            s.push(c);
            state.set_input(s);
        }

        _ => {}
    }

    Ok(false)
}

/// Submit the current input buffer and clear it.
///
/// If the buffer is empty or all-whitespace, does nothing.
/// On submission error, the error propagates to the caller; future iterations
/// could catch it and display an inline error message.
fn submit_input(state: &mut AppState, dirs: &AgentDirs) -> Result<(), TuiError> {
    let payload = state.input.trim().to_owned();
    if payload.is_empty() {
        return Ok(());
    }
    submit_message(&payload, dirs).map_err(TuiError::Submit)?;
    state.set_input(String::new());
    Ok(())
}
