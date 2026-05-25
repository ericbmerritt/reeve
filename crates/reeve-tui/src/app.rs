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
use crate::session::{self, Session};
use crate::state::{AppState, Screen};
use crate::submit::submit_message;
use crate::watcher::watch_tree;

/// Role name of the only chat the TUI can currently open. Phase 7 will
/// generalise the chat screen to other agents; until then, session
/// records that name any other agent are honoured only insofar as the
/// agent is registered and running — but the chat target falls back to
/// `LEAD_AGENT_NAME` regardless. Pinned as a const so the startup-flow
/// logic and the exit-write logic share one source of truth.
const LEAD_AGENT_NAME: &str = "lead";

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

// ── Session memory ────────────────────────────────────────────────────────────

/// Decide which screen to open on startup, given the session file and the
/// agent registry.
///
/// Returns [`Screen::Chat`] only when **all** of these hold:
/// - The session file records a `last_agent` value.
/// - That agent name is present in the agent registry.
/// - The registry marks the agent as Running (a stopped agent's chat is
///   not a useful landing place).
///
/// Otherwise returns [`Screen::Panopticon`]: no session, missing agent, or
/// non-running agent all collapse to "open the panopticon instead." This
/// is the operator-facing behaviour the spec mandates and the only
/// reasonable fallback when the recorded chat target is unreachable.
fn initial_screen_for_session(session: &Session, agent_registry_path: &Path) -> Screen {
    let Some(name) = session.last_agent.as_deref() else {
        return Screen::Panopticon;
    };
    let Ok(registry) = reeve_runtime::AgentRegistry::open(agent_registry_path.to_path_buf()) else {
        return Screen::Panopticon;
    };
    match registry.lookup(name) {
        Some(record) if matches!(record.status, reeve_runtime::AgentStatus::Running) => {
            Screen::Chat
        }
        _ => Screen::Panopticon,
    }
}

/// Write the session record on clean TUI exit. Per the spec, only chat-screen
/// exits should record `last_agent`; an exit from the panopticon must not
/// overwrite a valid prior chat record.
///
/// Errors are deliberately swallowed: the operator should never see a TUI
/// quit fail because of a session-file write hiccup. The worst-case
/// presentation is "next launch lands on the panopticon" — the same
/// fallback as a missing session file.
fn record_exit_to_session(session_path: &Path, state: &AppState) {
    if state.screen != Screen::Chat {
        return;
    }
    let session = Session {
        // Phase 6 only opens the lead chat, so the agent name we record is
        // always `LEAD_AGENT_NAME`. Phase 7 will replace this with the
        // actual focused agent.
        last_agent: Some(LEAD_AGENT_NAME.to_owned()),
    };
    let _ = session::write(session_path, &session);
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
#[expect(
    clippy::too_many_arguments,
    reason = "run is the multi-screen TUI entry point and now also threads \
              session-memory state (session_path) on top of the lead-chat \
              data sources, panopticon data sources, and operator credentials. \
              Bundling these into a context struct trades clarity for \
              indirection at the only non-test call sites (cmd_attach and \
              cmd_reeve in reeve-cli)."
)]
pub fn run(
    dirs: &AgentDirs,
    data_dir: &Path,
    agent_registry_path: &Path,
    session_path: &Path,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<(), TuiError> {
    let (mut terminal, _guard) = setup_terminal()?;

    let needs_reload = Arc::new(AtomicBool::new(true)); // true = load immediately on start
    let needs_reload_clone = Arc::clone(&needs_reload);

    // Watch the full `<data_dir>/agents/` tree so the panopticon sees
    // worker transitions (status/cost/conversation changes in any agent's
    // dir) and registry changes (a new spawn rewrites
    // `agents/registry.toml`). One recursive watch covers every agent
    // regardless of count — no inotify accounting per agent. Kept alive
    // until run() returns.
    let agents_root = data_dir.join("agents");
    let _watcher = watch_tree(&agents_root, move || {
        needs_reload_clone.store(true, Ordering::Release);
    })
    .map_err(TuiError::Watcher)?;

    let mut state = AppState::default();
    // Resolve the operator identity once at startup so inbound entries can
    // render with "you" for the operator and a distinct sender label for
    // worker/peer replies. If lookup fails the label falls back to a short
    // id, which is still better than the pre-attribution "you for everyone".
    state.operator_id = first_operator_id(registry);
    // Pick the starting screen from session memory: chat when the last
    // agent the operator was talking to is still running, panopticon
    // otherwise (no session, agent gone, agent stopped). The default
    // `Screen::Chat` on `AppState` is overridden here because the
    // panopticon-as-home story applies at startup; the field's default
    // exists for tests and constructed-in-place states.
    state.screen = initial_screen_for_session(&session::read(session_path), agent_registry_path);

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
                Screen::Quarantine => {
                    crate::ui_quarantine::draw(frame, &state.panopticon);
                }
            })
            .map_err(TuiError::Terminal)?;

        // Short timeout keeps watcher latency bounded.
        if event::poll(POLL_TIMEOUT).map_err(TuiError::Terminal)? {
            match event::read().map_err(TuiError::Terminal)? {
                Event::Key(key) => {
                    let prev_screen = state.screen;
                    if handle_key(key, &mut state, dirs, registry, keystore)? {
                        // Record the chat target the operator was on when
                        // they quit, so the next launch can resume it.
                        // Quitting from the panopticon does not write
                        // (would overwrite a valid prior chat record).
                        record_exit_to_session(session_path, &state);
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
            // Tab toggles Chat ↔ Panopticon. From Quarantine, Tab pops
            // back to the panopticon (its conceptual parent screen)
            // rather than entering the chat directly — keeps the
            // navigation structure shallow.
            match state.screen {
                Screen::Chat | Screen::Panopticon => state.toggle_screen(),
                Screen::Quarantine => state.screen = Screen::Panopticon,
            }
            return Ok(false);
        }
        (KeyCode::Esc, _) => match state.screen {
            Screen::Chat => return Ok(true),
            Screen::Panopticon => {
                state.screen = Screen::Chat;
                return Ok(false);
            }
            Screen::Quarantine => {
                state.screen = Screen::Panopticon;
                return Ok(false);
            }
        },
        _ => {}
    }

    match state.screen {
        Screen::Chat => handle_key_chat(key, state, dirs, registry, keystore),
        Screen::Panopticon => Ok(handle_key_panopticon(key, state)),
        Screen::Quarantine => Ok(handle_key_quarantine(key, state)),
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
        // `Q` opens the quarantine review. Phase 6 ships a stub renderer
        // that surfaces the existing quarantine count; Phase 8 fills in
        // the real per-message review UI. Crossterm typically delivers
        // Shift+q as `Char('Q')` (uppercase, modifier-less) on most
        // terminals, but a `SHIFT`-modifier variant is also possible —
        // accept both.
        (KeyCode::Char('Q'), _) => state.screen = Screen::Quarantine,
        _ => {}
    }
    false
}

/// Quarantine-screen key bindings. Today's stub has nothing screen-local
/// to do — the global `Esc`/`Tab`/`q` handler already covers back-to-
/// panopticon and quit. `Q` here is an explicit close-and-return so the
/// operator can toggle the screen with the same key that opened it.
/// Phase 8 fills in approve/release/discard actions here.
#[expect(
    clippy::single_match,
    reason = "match shape pre-claimed for Phase 8 approve/release/discard \
              arms; converting to `if let` now would just have to be \
              reverted when those land."
)]
fn handle_key_quarantine(key: event::KeyEvent, state: &mut AppState) -> bool {
    match (key.code, key.modifiers) {
        (KeyCode::Char('Q'), _) => state.screen = Screen::Panopticon,
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

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panopticon::{AgentRow, PanopticonSnapshot};
    use crate::state::AgentStatus;
    use time::{Duration, OffsetDateTime};

    fn key(code: KeyCode) -> event::KeyEvent {
        event::KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn state_with_agents(count: usize) -> AppState {
        let mut state = AppState::default();
        state.screen = Screen::Panopticon;
        state.panopticon = PanopticonSnapshot {
            agents: (0..count)
                .map(|i| AgentRow {
                    name: format!("agent-{i}"),
                    persona_name: None,
                    status: AgentStatus::Idle,
                    is_running: true,
                    cost_usd: 0.0,
                    elapsed: Duration::seconds(0),
                    state_changed_at: None,
                })
                .collect(),
            recent_events: Vec::new(),
            queue_counts: crate::panopticon::QueueCounts::default(),
            total_cost_usd: 0.0,
            session_elapsed: Some(Duration::seconds(0)),
        };
        state
    }

    // A1: `j` and Down both move focus down by one row; the helper clamps
    // at the last agent so spamming `j` cannot run off the end.
    #[test]
    fn handle_key_panopticon_j_and_down_move_focus_down_with_clamp() {
        let mut state = state_with_agents(3);
        assert!(!handle_key_panopticon(key(KeyCode::Char('j')), &mut state));
        assert_eq!(state.panopticon_focus, 1);
        assert!(!handle_key_panopticon(key(KeyCode::Down), &mut state));
        assert_eq!(state.panopticon_focus, 2);
        // Already at last agent; further presses clamp.
        assert!(!handle_key_panopticon(key(KeyCode::Down), &mut state));
        assert_eq!(state.panopticon_focus, 2);
    }

    // A2: `k` and Up both move focus up by one; saturate at zero.
    #[test]
    fn handle_key_panopticon_k_and_up_move_focus_up_with_clamp() {
        let mut state = state_with_agents(3);
        state.panopticon_focus = 2;
        assert!(!handle_key_panopticon(key(KeyCode::Char('k')), &mut state));
        assert_eq!(state.panopticon_focus, 1);
        assert!(!handle_key_panopticon(key(KeyCode::Up), &mut state));
        assert_eq!(state.panopticon_focus, 0);
        // Saturate at 0.
        assert!(!handle_key_panopticon(key(KeyCode::Up), &mut state));
        assert_eq!(state.panopticon_focus, 0);
    }

    // A3: Enter on the panopticon pops back to the chat screen — Phase 6
    // collapses every Enter to "return to lead chat" since only the lead's
    // chat is wired. Phase 7 will branch on the focused agent.
    #[test]
    fn handle_key_panopticon_enter_switches_to_chat() {
        let mut state = state_with_agents(2);
        assert!(!handle_key_panopticon(key(KeyCode::Enter), &mut state));
        assert_eq!(state.screen, Screen::Chat);
    }

    // A4: unrelated keys are no-ops; focus and screen stay put.
    #[test]
    fn handle_key_panopticon_ignores_unrelated_keys() {
        let mut state = state_with_agents(3);
        state.panopticon_focus = 1;
        let before = state.panopticon_focus;
        assert!(!handle_key_panopticon(key(KeyCode::Char('x')), &mut state));
        assert_eq!(state.panopticon_focus, before);
        assert_eq!(state.screen, Screen::Panopticon);
    }

    // A5: focus-down on an empty agent table is a no-op rather than an
    // overflow. Real users hit this transiently at startup (registry
    // empty, snapshot still rendering).
    #[test]
    fn handle_key_panopticon_focus_down_is_noop_on_empty_table() {
        let mut state = state_with_agents(0);
        assert!(!handle_key_panopticon(key(KeyCode::Char('j')), &mut state));
        assert_eq!(state.panopticon_focus, 0);
    }

    // A6: `Q` on the panopticon opens the quarantine review stub. This
    // is the one operator-facing key the queue-strip footer advertises.
    #[test]
    fn handle_key_panopticon_q_opens_quarantine() {
        let mut state = state_with_agents(2);
        assert!(!handle_key_panopticon(key(KeyCode::Char('Q')), &mut state));
        assert_eq!(state.screen, Screen::Quarantine);
    }

    // A7: `Q` on the quarantine screen closes back to the panopticon —
    // toggle-on / toggle-off with the same key. Matches the operator's
    // muscle memory for press-the-same-key-to-leave panels.
    #[test]
    fn handle_key_quarantine_q_returns_to_panopticon() {
        let mut state = state_with_agents(2);
        state.screen = Screen::Quarantine;
        assert!(!handle_key_quarantine(key(KeyCode::Char('Q')), &mut state,));
        assert_eq!(state.screen, Screen::Panopticon);
    }

    // A8: unrelated keys on quarantine are no-ops. The global handler
    // already covers Esc and Tab; the stub itself has no other actions
    // until Phase 8 wires approve/release/discard.
    #[test]
    fn handle_key_quarantine_ignores_unrelated_keys() {
        let mut state = state_with_agents(2);
        state.screen = Screen::Quarantine;
        assert!(!handle_key_quarantine(key(KeyCode::Char('x')), &mut state,));
        assert_eq!(state.screen, Screen::Quarantine);
    }

    // ── Session-driven startup screen ────────────────────────────────

    /// `AgentRegistry::open` enforces `0o700` on its parent directory;
    /// `tempdir()` creates with `0o755`. Apply the expected mode before
    /// opening so the registry doesn't reject the directory as
    /// misconfigured.
    #[cfg(unix)]
    fn chmod_700(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// Register `name` as a running agent in a fresh on-disk registry at
    /// `path` so `initial_screen_for_session` can find it.
    fn register_running_agent(path: &Path, name: &str) {
        use reeve_runtime::{
            AgentRecord, AgentRegistry, AgentStatus as RuntimeAgentStatus, ValidatedAgentName,
        };
        #[cfg(unix)]
        chmod_700(path.parent().unwrap());
        let mut registry = AgentRegistry::open(path.to_path_buf()).unwrap();
        registry
            .register(AgentRecord {
                name: ValidatedAgentName::new(name).unwrap(),
                identity_id: reeve_types::IdentityId::new().unwrap(),
                inbox_dir: path.parent().unwrap().join(name).join("inbox"),
                persona_name: Some(name.to_owned()),
                spawned_at: OffsetDateTime::now_utc(),
                status: RuntimeAgentStatus::Running,
            })
            .unwrap();
    }

    // SA1: no session → panopticon. The "first launch" path: the operator
    // has never quit a chat, so there's nothing to resume.
    #[test]
    fn initial_screen_with_no_session_is_panopticon() {
        let tmp = tempfile::tempdir().unwrap();
        // Build a registry path inside tmp but never write one — open will
        // succeed against the empty directory.
        let registry_path = tmp.path().join("registry.toml");
        let screen = initial_screen_for_session(&Session::default(), &registry_path);
        assert_eq!(screen, Screen::Panopticon);
    }

    // SA2: session names a running agent → that agent's chat. The "resume"
    // path the spec's done-when criterion calls out.
    #[test]
    fn initial_screen_with_running_agent_in_session_is_chat() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_path = tmp.path().join("registry.toml");
        register_running_agent(&registry_path, "lead");

        let screen = initial_screen_for_session(
            &Session {
                last_agent: Some("lead".to_owned()),
            },
            &registry_path,
        );
        assert_eq!(screen, Screen::Chat);
    }

    // SA3: session names an agent that is no longer in the registry →
    // panopticon. The registry was rebuilt, or the agent was unregistered;
    // we fall back to the global view rather than landing on a stale chat.
    #[test]
    fn initial_screen_with_unknown_agent_in_session_is_panopticon() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_path = tmp.path().join("registry.toml");
        register_running_agent(&registry_path, "lead");

        let screen = initial_screen_for_session(
            &Session {
                last_agent: Some("worker-gone".to_owned()),
            },
            &registry_path,
        );
        assert_eq!(screen, Screen::Panopticon);
    }

    // SA4: session names a stopped agent → panopticon. A stopped agent's
    // chat is not a useful landing place; the spec specifies the fallback.
    #[test]
    fn initial_screen_with_stopped_agent_in_session_is_panopticon() {
        use reeve_runtime::{
            AgentRecord, AgentRegistry, AgentStatus as RuntimeAgentStatus, ValidatedAgentName,
        };
        let tmp = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        chmod_700(tmp.path());
        let registry_path = tmp.path().join("registry.toml");
        let mut registry = AgentRegistry::open(registry_path.clone()).unwrap();
        registry
            .register(AgentRecord {
                name: ValidatedAgentName::new("lead").unwrap(),
                identity_id: reeve_types::IdentityId::new().unwrap(),
                inbox_dir: tmp.path().join("lead").join("inbox"),
                persona_name: Some("lead".to_owned()),
                spawned_at: OffsetDateTime::now_utc(),
                status: RuntimeAgentStatus::Stopped,
            })
            .unwrap();

        let screen = initial_screen_for_session(
            &Session {
                last_agent: Some("lead".to_owned()),
            },
            &registry_path,
        );
        assert_eq!(screen, Screen::Panopticon);
    }

    // SA5: unreadable registry path → panopticon. Best-effort: if the
    // registry can't be opened for any reason, fall back rather than
    // panic.
    #[test]
    fn initial_screen_with_unreadable_registry_is_panopticon() {
        let screen = initial_screen_for_session(
            &Session {
                last_agent: Some("lead".to_owned()),
            },
            Path::new("/nonexistent/no-such-registry.toml"),
        );
        assert_eq!(screen, Screen::Panopticon);
    }
}
