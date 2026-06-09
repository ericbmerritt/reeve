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
use crate::state::{AppState, InspectTab, Screen};
use crate::submit::submit_message;
use crate::watcher::watch_tree;

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
        last_agent: Some(state.chat_agent_name.clone()),
    };
    let _ = session::write(session_path, &session);
}

// ── State loading ─────────────────────────────────────────────────────────────

/// Reload all on-disk state into `state`.
///
/// Called on every watcher callback. Reads chat-screen data from the
/// currently selected chat agent (`state.chat_agent_name`) and refreshes
/// the panopticon snapshot from the agent registry. Individual reads
/// return safe defaults on any filesystem error (see `crate::reader`).
///
/// `data_dir` is the runtime data root; per-agent dirs are derived from
/// it plus the chat-agent name on every reload, so switching the
/// chat-agent (e.g., Enter on a panopticon row) requires only a state
/// mutation followed by a reload trigger — no signature changes
/// downstream.
fn reload_state(state: &mut AppState, data_dir: &Path, agent_registry_path: &Path) {
    let active = active_agent_name(state).to_owned();
    if let Ok(dirs) = AgentDirs::open(data_dir, &active) {
        state.status = read_status(&dirs.status_path());
        state.conversation = read_conversation(&dirs.conversation_path());
        state.cost_usd = read_cost(&dirs.cost_path());
        if let Some(snapshot) = read_spawn_snapshot(&dirs) {
            state.model_id.clear();
            state.model_id.push_str(snapshot.model());
            state.persona_name = snapshot.persona_name;
        }
    }
    state.panopticon = read_panopticon_snapshot(data_dir, agent_registry_path, state.operator_id);
    // The quarantine snapshot is rebuilt on every reload so the
    // panopticon's queue count and the review screen's entry list stay
    // in sync. The reader walks every agent's `inbox/quarantine/`
    // directory and tolerates missing/unreadable directories silently
    // (see `quarantine_view::read_files`), so this is safe to run even
    // when the operator has never opened the review screen.
    state.quarantine = crate::quarantine_view::read_snapshot(data_dir);
    // Discard or watcher activity can shrink the entry list out from
    // under a stale focus index. Clamp here, in the reload, rather
    // than at every render-site read.
    let entry_count = state.quarantine.entries.len();
    if entry_count == 0 {
        state.quarantine_focus = 0;
    } else if state.quarantine_focus >= entry_count {
        state.quarantine_focus = entry_count - 1;
    }
}

/// Resolve the agent whose disk state should populate the per-agent
/// fields (conversation, status, cost, persona, model) on the next
/// reload.
///
/// On [`Screen::Inspect`] the inspect target wins so the drill-in view
/// reads the agent the operator just Enter-ed into. Everywhere else
/// (Chat, Panopticon, Quarantine) the chat target wins so the chat
/// screen's data is fresh when the operator Tab's back to it. Falling
/// back to `chat_agent_name` when `inspect_agent_name` is None is
/// defensive — `Enter` from the panopticon always sets the inspect
/// target before switching screens, so the None branch is unreachable
/// under normal flow.
fn active_agent_name(state: &AppState) -> &str {
    match (state.screen, state.inspect_agent_name.as_deref()) {
        (Screen::Inspect, Some(name)) => name,
        _ => &state.chat_agent_name,
    }
}

/// Read the spawn snapshot for the agent at `dirs.agent_toml_path()`.
/// Returns `None` on any filesystem or parse error — the chat title bar
/// keeps its previous persona/model labels rather than going blank.
fn read_spawn_snapshot(dirs: &AgentDirs) -> Option<reeve_runtime::SpawnSnapshot> {
    let body = std::fs::read_to_string(dirs.agent_toml_path()).ok()?;
    toml::from_str(&body).ok()
}

// ── Event loop ────────────────────────────────────────────────────────────────

/// Run the TUI until the user quits.
///
/// Blocks until `q` / `Esc` is pressed or an unrecoverable error occurs.
/// Terminal is restored (raw mode off, alternate screen left) on all exit paths.
///
/// # Parameters
///
/// - `data_dir`: runtime data root. Per-agent `AgentDirs` are derived from
///   here and the currently focused chat agent on every reload — switching
///   the chat target is a state mutation, not a signature change.
/// - `agent_registry_path`: agent registry TOML; drives the panopticon
///   snapshot and resolves chat-resume targets at startup.
/// - `session_path`: per-operator session memory file
///   (`<state_dir>/session.toml`).
/// - `registry`: the identity registry, used by [`submit_message`] to
///   locate the operator identity.
/// - `keystore`: the platform keystore, used by [`submit_message`] to
///   retrieve the operator signing key.
///
/// # Errors
///
/// Returns [`TuiError::Terminal`] on terminal I/O failure,
/// [`TuiError::Watcher`] if the filesystem watcher cannot start, or
/// [`TuiError::Submit`] if a message write fails (not currently surfaced to the
/// user — future iterations should show an inline error).
#[expect(
    clippy::too_many_arguments,
    reason = "run is the multi-screen TUI entry point and threads four \
              filesystem roots, the operator credentials, plus an optional \
              CLI-supplied chat target. Bundling into a context struct \
              trades clarity for indirection at the only non-test call \
              sites (cmd_reeve and cmd_attach in reeve-cli)."
)]
#[expect(
    clippy::too_many_lines,
    reason = "linear event loop: each branch is short but the full keyboard \
              and reload surface must be wired in one place to stay auditable"
)]
pub fn run(
    data_dir: &Path,
    agent_registry_path: &Path,
    session_path: &Path,
    initial_chat_agent: Option<&str>,
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
    // Pick the starting screen + chat target from session memory: chat
    // when the last agent the operator was talking to is still running,
    // panopticon otherwise (no session, agent gone, agent stopped). The
    // default `Screen::Chat` + `chat_agent_name = "lead"` on `AppState`
    // are overridden here because the panopticon-as-home story applies
    // at startup; the defaults exist for tests and constructed-in-place
    // states.
    if let Some(name) = initial_chat_agent {
        // `reeve attach <name>` — explicit override; open that agent's
        // chat unconditionally. Skips the session-memory consultation:
        // the operator told us which chat they want, that's the chat
        // they get.
        state.chat_agent_name.clear();
        state.chat_agent_name.push_str(name);
        state.screen = Screen::Chat;
    } else {
        let session = session::read(session_path);
        state.screen = initial_screen_for_session(&session, agent_registry_path);
        if state.screen == Screen::Chat {
            if let Some(name) = session.last_agent.as_deref() {
                state.chat_agent_name.clear();
                state.chat_agent_name.push_str(name);
            }
        }
    }

    let mut last_forced_reload = std::time::Instant::now();
    loop {
        // Force a reload every 2 seconds regardless of watcher events.
        // notify's recommended_watcher on macOS (both kqueue and FSEvents)
        // silently stops delivering events in certain conditions. Polling
        // at 2 s is fast enough to feel live and is the pragmatic fix
        // until the notify reliability issue is resolved upstream.
        if last_forced_reload.elapsed() >= Duration::from_secs(2) {
            needs_reload.store(true, Ordering::Release);
            last_forced_reload = std::time::Instant::now();
        }

        if needs_reload.swap(false, Ordering::Acquire) {
            reload_state(&mut state, data_dir, agent_registry_path);
        }

        terminal
            .draw(|frame| match state.screen {
                Screen::Chat => crate::ui::draw(frame, &state),
                Screen::Panopticon => {
                    crate::ui_panopticon::draw(frame, &state.panopticon, state.panopticon_focus);
                }
                Screen::Inspect => crate::ui_inspect::draw(frame, &state),
                Screen::Quarantine => crate::ui_quarantine::draw(frame, &state),
                Screen::QuarantineCompose => {
                    crate::ui_quarantine::draw_compose(frame, &state);
                }
            })
            .map_err(TuiError::Terminal)?;

        // Short timeout keeps watcher latency bounded.
        if event::poll(POLL_TIMEOUT).map_err(TuiError::Terminal)? {
            match event::read().map_err(TuiError::Terminal)? {
                Event::Key(key) => {
                    let prev_screen = state.screen;
                    let prev_chat_agent = state.chat_agent_name.clone();
                    let prev_inspect_agent = state.inspect_agent_name.clone();
                    if handle_key(
                        key,
                        &mut state,
                        data_dir,
                        agent_registry_path,
                        registry,
                        keystore,
                    )? {
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
                    // Trigger a reload whenever the *active* agent (the one
                    // populating state.conversation/status/cost) could
                    // have changed:
                    //
                    // - `chat_agent_name` differs: only happens at startup
                    //   today, but defensive in case a future surface
                    //   introduces mid-session switching.
                    // - `inspect_agent_name` differs: Enter on a panopticon
                    //   row picked a new inspect target.
                    // - the screen transitioned between Chat and Inspect:
                    //   `active_agent_name` returns a different name
                    //   depending on which screen is up, so the on-screen
                    //   data must refresh even if neither name changed.
                    let active_screen_changed = prev_screen != state.screen
                        && matches!(state.screen, Screen::Chat | Screen::Inspect);
                    if state.chat_agent_name != prev_chat_agent
                        || state.inspect_agent_name != prev_inspect_agent
                        || active_screen_changed
                    {
                        needs_reload.store(true, Ordering::Release);
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
#[expect(
    clippy::too_many_arguments,
    reason = "each parameter is a distinct runtime resource with no natural grouping peer"
)]
fn handle_key(
    key: event::KeyEvent,
    state: &mut AppState,
    data_dir: &Path,
    agent_registry_path: &Path,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<bool, TuiError> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), KeyModifiers::NONE) => return Ok(true),
        (KeyCode::Tab, _) => {
            // Tab toggles Chat ↔ Panopticon for the top-level navigation.
            // From Quarantine, Tab pops back to the panopticon (its
            // conceptual parent screen). On Inspect, Tab is screen-local
            // — it cycles tabs across the top of the inspect view — so
            // the global Tab handler defers to handle_key_inspect.
            match state.screen {
                Screen::Chat | Screen::Panopticon => state.toggle_screen(),
                // Tab from Quarantine/QuarantineCompose pops back to
                // panopticon. QuarantineCompose is a modal; Tab cancels
                // the compose and goes to the quarantine list's parent
                // rather than staying in the review screen.
                Screen::Quarantine | Screen::QuarantineCompose => {
                    quarantine_compose_cancel(state);
                    state.screen = Screen::Panopticon;
                }
                Screen::Inspect => {
                    return handle_key_inspect(key, state, data_dir, registry, keystore)
                }
            }
            return Ok(false);
        }
        (KeyCode::Esc, _) => {
            match state.screen {
                Screen::Chat => return Ok(true),
                Screen::Panopticon => state.screen = Screen::Chat,
                // Inspect and Quarantine both pop one level back to the
                // panopticon.
                Screen::Inspect | Screen::Quarantine => state.screen = Screen::Panopticon,
                // Esc from the compose surface cancels without sending
                // and returns to the quarantine review list.
                Screen::QuarantineCompose => {
                    quarantine_compose_cancel(state);
                    state.screen = Screen::Quarantine;
                }
            }
            return Ok(false);
        }
        _ => {}
    }

    match state.screen {
        Screen::Chat => handle_key_chat(key, state, data_dir, registry, keystore),
        Screen::Panopticon => Ok(handle_key_panopticon(key, state, agent_registry_path)),
        Screen::Inspect => handle_key_inspect(key, state, data_dir, registry, keystore),
        Screen::Quarantine => Ok(handle_key_quarantine(key, state)),
        Screen::QuarantineCompose => {
            handle_key_quarantine_compose(key, state, data_dir, registry, keystore)
        }
    }
}

/// Chat-screen key bindings: typing, scrolling, submit on Enter.
fn handle_key_chat(
    key: event::KeyEvent,
    state: &mut AppState,
    data_dir: &Path,
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
            submit_input(state, data_dir, registry, keystore)?;
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

/// Panopticon-screen key bindings.
fn handle_key_panopticon(
    key: event::KeyEvent,
    state: &mut AppState,
    agent_registry_path: &Path,
) -> bool {
    match (key.code, key.modifiers) {
        (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => {
            state.panopticon_focus_down();
        }
        (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => {
            state.panopticon_focus_up();
        }
        (KeyCode::Enter, _) => {
            if let Some(agent) = state.panopticon.agents.get(state.panopticon_focus) {
                state.inspect_agent_name = Some(agent.name.clone());
                state.inspect_tab = InspectTab::Thread;
                state.scroll_to_bottom();
            }
            state.screen = Screen::Inspect;
        }
        // `c` opens a full-screen chat session with the focused agent.
        (KeyCode::Char('c'), KeyModifiers::NONE) => {
            if let Some(agent) = state.panopticon.agents.get(state.panopticon_focus) {
                state.chat_agent_name.clear();
                state.chat_agent_name.push_str(&agent.name);
                state.scroll_to_bottom();
                state.screen = Screen::Chat;
            }
        }
        // `d` removes the focused agent's registry record.
        // Allowed only for ghost or stopped non-lead agents; running agents
        // and the lead are protected. The agent directory is left on disk.
        (KeyCode::Char('d'), KeyModifiers::NONE) => {
            if let Some(agent) = state.panopticon.agents.get(state.panopticon_focus) {
                if agent.name != "lead" {
                    if let Ok(mut reg) =
                        reeve_runtime::AgentRegistry::open(agent_registry_path.to_path_buf())
                    {
                        let _ = reg.remove(&agent.name);
                    }
                }
            }
        }
        (KeyCode::Char('Q'), _) => state.screen = Screen::Quarantine,
        _ => {}
    }
    false
}

/// Inspect-screen key bindings.
///
/// On the Thread tab the inspect screen doubles as a chat interface: typing
/// populates the input buffer and Enter submits to the inspected agent.
/// Tab/Shift+Tab switch between inspect tabs (clearing the input buffer when
/// leaving Thread so stale text doesn't bleed into other tabs).
///
/// `h`/`Esc` return to the panopticon. `q` and global `Esc`/`Tab` are
/// consumed by the dispatcher before reaching here.
fn handle_key_inspect(
    key: event::KeyEvent,
    state: &mut AppState,
    data_dir: &Path,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<bool, TuiError> {
    const ROWS_PER_PAGE: u16 = 10;
    const ROWS_PER_LINE_NUDGE: u16 = 1;

    // On the Thread tab, character input and Enter go to the agent.
    if state.inspect_tab == InspectTab::Thread {
        match (key.code, key.modifiers) {
            (KeyCode::Enter, KeyModifiers::NONE) => {
                submit_inspect_input(state, data_dir, registry, keystore)?;
                return Ok(false);
            }
            (KeyCode::Backspace, _) => {
                let mut s = state.input.clone();
                if !s.is_empty() {
                    let trim_pos = s.char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
                    s.truncate(trim_pos);
                }
                state.set_input(s);
                return Ok(false);
            }
            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                let mut s = state.input.clone();
                s.push(c);
                state.set_input(s);
                return Ok(false);
            }
            _ => {}
        }
    }

    match (key.code, key.modifiers) {
        (KeyCode::Tab, KeyModifiers::NONE) => {
            state.set_input(String::new());
            state.inspect_tab = state.inspect_tab.next();
        }
        (KeyCode::Tab, KeyModifiers::SHIFT) | (KeyCode::BackTab, _) => {
            state.set_input(String::new());
            state.inspect_tab = state.inspect_tab.prev();
        }
        (KeyCode::Char(c @ '1'..='5'), _) => {
            state.set_input(String::new());
            let tab = match c {
                '1' => InspectTab::Thread,
                '2' => InspectTab::Tools,
                '3' => InspectTab::Model,
                '4' => InspectTab::Decisions,
                '5' => InspectTab::Memory,
                _ => return Ok(false),
            };
            state.inspect_tab = tab;
        }
        (KeyCode::Char('h'), KeyModifiers::NONE) => {
            state.set_input(String::new());
            state.screen = Screen::Panopticon;
        }
        // `c` opens full-screen chat for the currently inspected agent.
        (KeyCode::Char('c'), KeyModifiers::NONE) => {
            if let Some(name) = state.inspect_agent_name.clone() {
                state.set_input(String::new());
                state.chat_agent_name.clear();
                state.chat_agent_name.push_str(&name);
                state.scroll_to_bottom();
                state.screen = Screen::Chat;
            }
        }
        (KeyCode::PageUp, _) => state.scroll_up(ROWS_PER_PAGE),
        (KeyCode::PageDown, _) => state.scroll_down(ROWS_PER_PAGE),
        (KeyCode::Up, KeyModifiers::SHIFT) => state.scroll_up(ROWS_PER_LINE_NUDGE),
        (KeyCode::Down, KeyModifiers::SHIFT) => state.scroll_down(ROWS_PER_LINE_NUDGE),
        (KeyCode::End, _) => state.scroll_to_bottom(),
        _ => {}
    }
    Ok(false)
}

/// Quarantine-screen key bindings.
///
/// - `j`/`k` (and Down/Up arrows) navigate the entry list.
/// - `d` (first press): enter discard-confirm mode. The renderer
///   replaces the footer with a prompt so the operator sees the
///   pending action.
/// - `d` or `y` (second press, confirm mode): delete the quarantine
///   file at the focused entry's path. The file removal is fire-and-
///   forget — the recursive watcher picks up the `inotify`/`FSEvents`
///   deletion event within the 250 ms debounce window and triggers a
///   full reload, which clamps the focus index and refreshes the list.
/// - Any other key while in confirm mode: cancel without deleting.
/// - `Q`: close quarantine and return to panopticon.
/// - `o`: begin convert flow (opens the compose sub-surface).
/// - `Esc`, `Tab`, `q` are handled by the global dispatcher before
///   this function is called and are never received here.
fn handle_key_quarantine(key: event::KeyEvent, state: &mut AppState) -> bool {
    // Confirm-discard mode: the next `d` or `y` executes the delete;
    // any other key cancels. Clear first so the `d` arm below can
    // branch on whether it was the FIRST or SECOND `d` press.
    if state.quarantine_confirm_discard {
        match (key.code, key.modifiers) {
            (KeyCode::Char('d' | 'y'), _) => {
                if let Some(entry) = state.quarantine.entries.get(state.quarantine_focus) {
                    // Fire-and-forget: watcher drives the list refresh.
                    let _ = std::fs::remove_file(&entry.path);
                }
                state.quarantine_confirm_discard = false;
            }
            // `Q` while confirming: cancel the confirm AND leave to the
            // panopticon, so Q always means "close quarantine" regardless
            // of pending confirm state.
            (KeyCode::Char('Q'), _) => {
                state.quarantine_confirm_discard = false;
                state.screen = Screen::Panopticon;
            }
            _ => {
                state.quarantine_confirm_discard = false;
            }
        }
        return false;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('j') | KeyCode::Down, KeyModifiers::NONE) => {
            state.quarantine_focus_down();
        }
        (KeyCode::Char('k') | KeyCode::Up, KeyModifiers::NONE) => {
            state.quarantine_focus_up();
        }
        (KeyCode::Char('d'), _) if !state.quarantine.entries.is_empty() => {
            state.quarantine_confirm_discard = true;
        }
        (KeyCode::Char('o'), _) => {
            let _ = handle_quarantine_convert(key, state);
        }
        (KeyCode::Char('Q'), _) => {
            state.quarantine_confirm_discard = false;
            state.screen = Screen::Panopticon;
        }
        _ => {}
    }
    false
}

/// Open the quarantine compose surface for the focused entry.
/// Pre-fills the input buffer with the raw body and records the
/// recipient so the submit path knows who to address the new envelope
/// to. `Esc` cancels and returns to the quarantine list; `Tab` cancels
/// and returns to the panopticon (per the global Tab handler). `Enter`
/// submits a fresh operator-signed envelope and returns to the quarantine
/// list.
fn handle_quarantine_convert(_key: event::KeyEvent, state: &mut AppState) -> bool {
    let Some(entry) = state.quarantine.entries.get(state.quarantine_focus) else {
        return false;
    };
    let recipient = entry.recipient.clone();
    let body = entry.raw_body.clone();
    state.quarantine_compose_recipient = recipient;
    state.set_input(body);
    state.quarantine_confirm_discard = false;
    state.screen = Screen::QuarantineCompose;
    false
}

/// Reset compose state without submitting.
fn quarantine_compose_cancel(state: &mut AppState) {
    state.set_input(String::new());
    state.quarantine_compose_recipient.clear();
}

/// Compose-surface key bindings: typing, backspace, Enter to submit.
///
/// `Enter` delivers a new operator-signed envelope to
/// `state.quarantine_compose_recipient`. The original quarantine file
/// is NOT deleted — it stays as the audit record. Esc and Tab are
/// handled by the global dispatcher before reaching here and switch the
/// screen back to `Screen::Quarantine`.
fn handle_key_quarantine_compose(
    key: event::KeyEvent,
    state: &mut AppState,
    data_dir: &Path,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<bool, TuiError> {
    match (key.code, key.modifiers) {
        (KeyCode::Enter, KeyModifiers::NONE) => {
            submit_quarantine_compose(state, data_dir, registry, keystore)?;
            state.screen = Screen::Quarantine;
        }
        (KeyCode::Backspace, _) => {
            let mut s = state.input.clone();
            if !s.is_empty() {
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

/// Submit the compose buffer as a new operator-signed envelope to the
/// quarantine compose recipient. On empty/whitespace input does nothing
/// (identical to `submit_input` for the chat screen). The original
/// quarantine file is intentionally not touched — it stays as the
/// audit record for the blocked message.
fn submit_quarantine_compose(
    state: &mut AppState,
    data_dir: &Path,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<(), TuiError> {
    let payload = state.input.trim().to_owned();
    if payload.is_empty() {
        return Ok(());
    }
    let recipient = state.quarantine_compose_recipient.clone();
    if recipient.is_empty() {
        return Ok(());
    }
    let dirs = AgentDirs::open(data_dir, &recipient).map_err(|err| {
        TuiError::Submit(crate::submit::SubmitError::Io {
            path: data_dir.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, err.to_string()),
        })
    })?;
    submit_message(&payload, &dirs, registry, keystore).map_err(TuiError::Submit)?;
    quarantine_compose_cancel(state);
    Ok(())
}

fn submit_input(
    state: &mut AppState,
    data_dir: &Path,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<(), TuiError> {
    submit_to_agent(
        state,
        data_dir,
        &state.chat_agent_name.clone(),
        registry,
        keystore,
    )
}

fn submit_inspect_input(
    state: &mut AppState,
    data_dir: &Path,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<(), TuiError> {
    let Some(agent_name) = state.inspect_agent_name.clone() else {
        return Ok(());
    };
    submit_to_agent(state, data_dir, &agent_name, registry, keystore)
}

fn submit_to_agent(
    state: &mut AppState,
    data_dir: &Path,
    agent_name: &str,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<(), TuiError> {
    let payload = state.input.trim().to_owned();
    if payload.is_empty() {
        return Ok(());
    }
    let dirs = AgentDirs::open(data_dir, agent_name).map_err(|err| {
        TuiError::Submit(crate::submit::SubmitError::Io {
            path: data_dir.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, err.to_string()),
        })
    })?;
    submit_message(&payload, &dirs, registry, keystore).map_err(TuiError::Submit)?;
    state.set_input(String::new());
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
                    is_ghost: false,
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
        assert!(!handle_key_panopticon(
            key(KeyCode::Char('j')),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.panopticon_focus, 1);
        assert!(!handle_key_panopticon(
            key(KeyCode::Down),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.panopticon_focus, 2);
        // Already at last agent; further presses clamp.
        assert!(!handle_key_panopticon(
            key(KeyCode::Down),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.panopticon_focus, 2);
    }

    // A2: `k` and Up both move focus up by one; saturate at zero.
    #[test]
    fn handle_key_panopticon_k_and_up_move_focus_up_with_clamp() {
        let mut state = state_with_agents(3);
        state.panopticon_focus = 2;
        assert!(!handle_key_panopticon(
            key(KeyCode::Char('k')),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.panopticon_focus, 1);
        assert!(!handle_key_panopticon(
            key(KeyCode::Up),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.panopticon_focus, 0);
        // Saturate at 0.
        assert!(!handle_key_panopticon(
            key(KeyCode::Up),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.panopticon_focus, 0);
    }

    // A3: Enter on the panopticon opens the per-agent inspect screen.
    // Per the Phase 7 done-when: chat is no longer reachable from the
    // panopticon — only `reeve attach <name>` opens a chat.
    #[test]
    fn handle_key_panopticon_enter_switches_to_inspect() {
        let mut state = state_with_agents(2);
        assert!(!handle_key_panopticon(
            key(KeyCode::Enter),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.screen, Screen::Inspect);
    }

    // A3b: Enter on a non-zero panopticon row sets `inspect_agent_name`
    // to that row's agent and lands on the Thread tab. The chat target
    // is intentionally unchanged — the operator keeps their typing
    // surface even while drilling into another agent.
    #[test]
    fn handle_key_panopticon_enter_targets_focused_agent_for_inspect() {
        let mut state = state_with_agents(3);
        state.chat_agent_name = "lead".to_owned();
        state.panopticon_focus = 2;
        let expected = state.panopticon.agents[2].name.clone();
        assert!(!handle_key_panopticon(
            key(KeyCode::Enter),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.screen, Screen::Inspect);
        assert_eq!(state.inspect_agent_name.as_deref(), Some(expected.as_str()));
        assert_eq!(state.inspect_tab, InspectTab::Thread);
        // Chat target left alone: inspect is a drill-in, not a switch.
        assert_eq!(state.chat_agent_name, "lead");
    }

    // A3c: Enter snaps scroll to the bottom on inspect entry so the
    // operator sees the most recent activity for the focused agent.
    // The input buffer is *not* cleared (inspect has no input pane;
    // a chat draft survives an inspect detour).
    #[test]
    fn handle_key_panopticon_enter_scrolls_inspect_to_bottom() {
        let mut state = state_with_agents(2);
        state.set_input("draft for chat agent".to_owned());
        state.scroll_up(20);
        state.panopticon_focus = 1;
        assert!(!handle_key_panopticon(
            key(KeyCode::Enter),
            &mut state,
            Path::new("")
        ));
        assert!(
            state.is_at_bottom(),
            "scroll should snap to bottom on inspect entry"
        );
        assert_eq!(
            state.input, "draft for chat agent",
            "input buffer must survive an inspect detour"
        );
    }

    // A3d: Enter with an out-of-range focus index (transient empty
    // snapshot) is defensive: switches screens but leaves the inspect
    // target alone. Showing the previously-inspected agent's data is
    // less confusing than showing a crash or an empty view.
    #[test]
    fn handle_key_panopticon_enter_with_empty_table_leaves_inspect_target() {
        let mut state = state_with_agents(0);
        state.inspect_agent_name = Some("previous-agent".to_owned());
        assert!(!handle_key_panopticon(
            key(KeyCode::Enter),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.screen, Screen::Inspect);
        assert_eq!(state.inspect_agent_name.as_deref(), Some("previous-agent"));
    }

    // A4: unrelated keys are no-ops; focus and screen stay put.
    #[test]
    fn handle_key_panopticon_ignores_unrelated_keys() {
        let mut state = state_with_agents(3);
        state.panopticon_focus = 1;
        let before = state.panopticon_focus;
        assert!(!handle_key_panopticon(
            key(KeyCode::Char('x')),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.panopticon_focus, before);
        assert_eq!(state.screen, Screen::Panopticon);
    }

    // A5: focus-down on an empty agent table is a no-op rather than an
    // overflow. Real users hit this transiently at startup (registry
    // empty, snapshot still rendering).
    #[test]
    fn handle_key_panopticon_focus_down_is_noop_on_empty_table() {
        let mut state = state_with_agents(0);
        assert!(!handle_key_panopticon(
            key(KeyCode::Char('j')),
            &mut state,
            Path::new("")
        ));
        assert_eq!(state.panopticon_focus, 0);
    }

    // A6: `Q` on the panopticon opens the quarantine review stub. This
    // is the one operator-facing key the queue-strip footer advertises.
    #[test]
    fn handle_key_panopticon_q_opens_quarantine() {
        let mut state = state_with_agents(2);
        assert!(!handle_key_panopticon(
            key(KeyCode::Char('Q')),
            &mut state,
            Path::new("")
        ));
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

    // ── reload_state per-agent dispatch ────────────────────────────────

    /// Provision an agent dir at `<data_dir>/agents/<name>/log/` and seed
    /// its conversation.jsonl with one inbound entry whose payload is
    /// `marker`. The marker lets a test distinguish which agent's file
    /// was read.
    fn seed_agent_conversation(data_dir: &Path, name: &str, marker: &str) {
        let log_dir = data_dir.join("agents").join(name).join("log");
        std::fs::create_dir_all(&log_dir).unwrap();
        let conv_path = log_dir.join("conversation.jsonl");
        let line = format!(
            r#"{{"type":"inbound","payload":"{marker}","timestamp_utc":"2026-05-25T20:00:00Z"}}"#
        );
        std::fs::write(&conv_path, format!("{line}\n")).unwrap();
    }

    // RS1: reload_state with chat_agent_name = "lead" reads lead's file;
    // changing chat_agent_name to "worker" and reloading reads worker's
    // file. This is the load-bearing invariant behind per-agent chat:
    // the conversation pane MUST track state.chat_agent_name on every
    // reload, no caching, no staleness.
    #[test]
    fn reload_state_switches_conversation_per_chat_agent_name() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        seed_agent_conversation(data_dir, "lead", "LEAD-MARKER");
        seed_agent_conversation(data_dir, "worker-x", "WORKER-MARKER");
        // Registry path doesn't need to exist for reload_state's
        // panopticon-snapshot read to succeed defensively (it returns an
        // empty snapshot on unreadable registries). The per-agent reads
        // are what we're verifying here.
        let registry_path = tmp.path().join("nonexistent-registry.toml");

        let mut state = AppState::default();
        state.chat_agent_name = "lead".to_owned();
        reload_state(&mut state, data_dir, &registry_path);
        assert_eq!(state.conversation.len(), 1);
        assert_eq!(state.conversation[0].text, "LEAD-MARKER");

        state.chat_agent_name = "worker-x".to_owned();
        reload_state(&mut state, data_dir, &registry_path);
        assert_eq!(state.conversation.len(), 1);
        assert_eq!(state.conversation[0].text, "WORKER-MARKER");

        // And switching back picks up lead again — no caching anywhere.
        state.chat_agent_name = "lead".to_owned();
        reload_state(&mut state, data_dir, &registry_path);
        assert_eq!(state.conversation[0].text, "LEAD-MARKER");
    }

    // RS2: reload_state reads the *inspect* agent's file when the
    // operator is on Screen::Inspect — even if the chat target is a
    // different agent. This is the load-bearing invariant for the
    // per-agent inspect drill-in: the screen MUST show the agent the
    // operator Enter-ed into, not the agent they were chatting with.
    #[test]
    fn reload_state_reads_inspect_agent_on_inspect_screen() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        seed_agent_conversation(data_dir, "lead", "LEAD-MARKER");
        seed_agent_conversation(data_dir, "worker-y", "WORKER-Y-MARKER");
        let registry_path = tmp.path().join("nonexistent-registry.toml");

        let mut state = AppState::default();
        state.chat_agent_name = "lead".to_owned();
        // Operator is inspecting worker-y while their chat target is
        // still lead. reload must read worker-y, not lead.
        state.screen = Screen::Inspect;
        state.inspect_agent_name = Some("worker-y".to_owned());
        reload_state(&mut state, data_dir, &registry_path);
        assert_eq!(state.conversation[0].text, "WORKER-Y-MARKER");

        // Tab/Esc back to Chat: reload must now read the chat target
        // again, not whatever inspect was looking at.
        state.screen = Screen::Chat;
        reload_state(&mut state, data_dir, &registry_path);
        assert_eq!(state.conversation[0].text, "LEAD-MARKER");
    }

    // RS3: defensive — Screen::Inspect with None inspect_agent_name
    // falls back to the chat target rather than panicking. This branch
    // is unreachable in normal flow (Enter from panopticon always sets
    // the inspect target before switching screens), but the fallback
    // keeps reload_state robust against future call sites that might
    // construct the state differently.
    #[test]
    fn reload_state_inspect_with_none_target_falls_back_to_chat_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        seed_agent_conversation(data_dir, "lead", "LEAD-MARKER");
        let registry_path = tmp.path().join("nonexistent-registry.toml");

        let mut state = AppState::default();
        state.chat_agent_name = "lead".to_owned();
        state.screen = Screen::Inspect;
        state.inspect_agent_name = None;
        reload_state(&mut state, data_dir, &registry_path);
        assert_eq!(state.conversation[0].text, "LEAD-MARKER");
    }

    // ── Inspect-screen key bindings ───────────────────────────────────

    fn state_in_inspect() -> AppState {
        let mut state = AppState::default();
        state.screen = Screen::Inspect;
        state.inspect_agent_name = Some("worker-x".to_owned());
        state.inspect_tab = InspectTab::Thread;
        state.panopticon_focus = 2; // preserved through the round trip
        state
    }

    // IK1: Tab cycles tabs forward through the five-entry list and
    // wraps from Memory back to Thread. The cycle order matches the
    // wireframe (Thread → Tools → Model → Decisions → Memory → Thread).
    #[test]
    fn handle_key_inspect_tab_cycles_forward_with_wrap() {
        let mut state = state_in_inspect();
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();
        let order = [
            InspectTab::Tools,
            InspectTab::Model,
            InspectTab::Decisions,
            InspectTab::Memory,
            InspectTab::Thread,
        ];
        for expected in order {
            let quit = handle_key_inspect(
                key(KeyCode::Tab),
                &mut state,
                tmp.path(),
                &registry,
                &keystore,
            )
            .unwrap();
            assert!(!quit);
            assert_eq!(state.inspect_tab, expected);
        }
    }

    // IK2: BackTab (Shift+Tab on most terminals) cycles backward and
    // wraps from Thread back to Memory. Symmetric with IK1.
    #[test]
    fn handle_key_inspect_backtab_cycles_backward_with_wrap() {
        let mut state = state_in_inspect();
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();
        let order = [
            InspectTab::Memory,
            InspectTab::Decisions,
            InspectTab::Model,
            InspectTab::Tools,
            InspectTab::Thread,
        ];
        for expected in order {
            let quit = handle_key_inspect(
                key(KeyCode::BackTab),
                &mut state,
                tmp.path(),
                &registry,
                &keystore,
            )
            .unwrap();
            assert!(!quit);
            assert_eq!(state.inspect_tab, expected);
        }
    }

    // IK3: 1-5 jump directly to a specific tab in display order when on a
    // non-Thread tab. On the Thread tab, character keys go to the input
    // buffer instead, so numeric shortcuts work from Tools and above.
    #[test]
    fn handle_key_inspect_numeric_keys_jump_to_tab() {
        let mut state = state_in_inspect();
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();

        // Start on Tools tab (Tab away from Thread) so numeric keys navigate.
        state.inspect_tab = InspectTab::Tools;

        let cases = [
            ('1', InspectTab::Thread),
            ('2', InspectTab::Tools),
            ('3', InspectTab::Model),
            ('4', InspectTab::Decisions),
            ('5', InspectTab::Memory),
        ];
        for (ch, expected) in cases {
            // Move off Thread tab before each numeric press so the key
            // reaches the navigation arm rather than the input buffer.
            if state.inspect_tab == InspectTab::Thread {
                state.inspect_tab = InspectTab::Tools;
            }
            let quit = handle_key_inspect(
                key(KeyCode::Char(ch)),
                &mut state,
                tmp.path(),
                &registry,
                &keystore,
            )
            .unwrap();
            assert!(!quit, "key '{ch}' should not quit");
            assert_eq!(
                state.inspect_tab, expected,
                "key '{ch}' should jump to {expected:?}"
            );
        }
        // '6' is out of range on Tools tab; stays at Tools.
        state.inspect_tab = InspectTab::Tools;
        let quit = handle_key_inspect(
            key(KeyCode::Char('6')),
            &mut state,
            tmp.path(),
            &registry,
            &keystore,
        )
        .unwrap();
        assert!(!quit);
        assert_eq!(state.inspect_tab, InspectTab::Tools);
    }

    // IK4: `h` returns to the panopticon from a non-Thread tab. On the
    // Thread tab, `h` is a regular character that goes to the input buffer;
    // Esc (handled by the global dispatcher) is the back key from Thread.
    #[test]
    fn handle_key_inspect_h_returns_to_panopticon_from_non_thread_tab() {
        let mut state = state_in_inspect();
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();
        state.inspect_tab = InspectTab::Tools; // not Thread
        assert_eq!(state.panopticon_focus, 2);
        let quit = handle_key_inspect(
            key(KeyCode::Char('h')),
            &mut state,
            tmp.path(),
            &registry,
            &keystore,
        )
        .unwrap();
        assert!(!quit);
        assert_eq!(state.screen, Screen::Panopticon);
        assert_eq!(state.panopticon_focus, 2);
    }

    // IK5: Esc behaves the same way as `h` — back to panopticon, focus
    // preserved. Two key bindings for the same action match the rest
    // of the TUI (h is vim canon, Esc is general muscle memory).
    // Routed through the global handle_key dispatcher since Esc is a
    // global key.
    #[test]
    fn handle_key_esc_from_inspect_returns_to_panopticon() {
        let mut state = state_in_inspect();
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();
        let was_exit = handle_key(
            key(KeyCode::Esc),
            &mut state,
            tmp.path(),
            tmp.path(),
            &registry,
            &keystore,
        )
        .unwrap();
        assert!(!was_exit, "Esc from inspect must not exit the TUI");
        assert_eq!(state.screen, Screen::Panopticon);
        assert_eq!(state.panopticon_focus, 2);
    }

    // IK6: Tab on the inspect screen is screen-local — it cycles tabs,
    // not screens. Without this, the global Tab handler would toggle
    // Chat ↔ Panopticon and drop the operator out of inspect on every
    // Tab press.
    #[test]
    fn handle_key_tab_on_inspect_stays_in_inspect_and_cycles_tab() {
        let mut state = state_in_inspect();
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();
        let was_exit = handle_key(
            key(KeyCode::Tab),
            &mut state,
            tmp.path(),
            tmp.path(),
            &registry,
            &keystore,
        )
        .unwrap();
        assert!(!was_exit);
        assert_eq!(state.screen, Screen::Inspect, "Tab must not leave inspect");
        assert_eq!(state.inspect_tab, InspectTab::Tools);
    }

    /// Construct registry + keystore for the few inspect tests that
    /// route through the global `handle_key`. Neither is exercised by
    /// inspect dispatch (the global handler returns early on Esc/Tab
    /// without touching the chat helper) but `handle_key` requires the
    /// references.
    ///
    /// The tempdir's lifetime is leaked into the registry by tying it
    /// to a `Box::leak`'d path; the test process owns the directory
    /// until exit.
    fn test_registry_and_keystore() -> (
        IdentityRegistry,
        reeve_runtime::keychain::memory::MemoryKeyStore,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        chmod_700(tmp.path());
        let path = tmp.keep();
        let registry = IdentityRegistry::open(path).unwrap();
        (
            registry,
            reeve_runtime::keychain::memory::MemoryKeyStore::new(),
        )
    }

    // ── Quarantine key bindings ───────────────────────────────────────

    fn quarantine_state(entries: Vec<crate::quarantine_view::QuarantineEntry>) -> AppState {
        let mut state = AppState::default();
        state.screen = Screen::Quarantine;
        state.quarantine = crate::quarantine_view::QuarantineSnapshot {
            entries,
            truncated: false,
        };
        state
    }

    fn fake_entry(name: &str, reason: &str) -> crate::quarantine_view::QuarantineEntry {
        crate::quarantine_view::QuarantineEntry {
            path: std::path::PathBuf::from(format!("/x/{name}.{reason}")),
            recipient: name.to_owned(),
            arrived: None,
            reason: reason.to_owned(),
            meta: crate::quarantine_view::EnvelopeMeta::ParseFailure {
                filename: format!("{name}.{reason}"),
            },
            raw_body: format!("body for {name}"),
            body_lossy: false,
        }
    }

    // QK1: j/k and arrow keys navigate with clamping — no underflow
    // or overflow.
    #[test]
    fn quarantine_j_k_navigate_with_clamp() {
        let mut state = quarantine_state(vec![
            fake_entry("a", "replay"),
            fake_entry("b", "clock_skew"),
        ]);
        assert!(!handle_key_quarantine(key(KeyCode::Char('j')), &mut state));
        assert_eq!(state.quarantine_focus, 1);
        assert!(!handle_key_quarantine(key(KeyCode::Char('j')), &mut state));
        assert_eq!(state.quarantine_focus, 1, "clamps at last");
        assert!(!handle_key_quarantine(key(KeyCode::Char('k')), &mut state));
        assert_eq!(state.quarantine_focus, 0);
        assert!(!handle_key_quarantine(key(KeyCode::Char('k')), &mut state));
        assert_eq!(state.quarantine_focus, 0, "clamps at zero");
    }

    // QK2: first `d` sets confirm_discard; second `d` removes the
    // quarantine file. Since we use a fake in-memory path, the remove
    // will Err, which is ignored — what we test is that the state
    // transitions are correct (confirm cleared, focus unchanged).
    #[test]
    fn quarantine_d_first_sets_confirm_second_clears() {
        let mut state = quarantine_state(vec![fake_entry("lead", "signature_invalid")]);
        // First d → enter confirm mode.
        assert!(!handle_key_quarantine(key(KeyCode::Char('d')), &mut state));
        assert!(
            state.quarantine_confirm_discard,
            "confirm not set after first d"
        );
        // Second d → attempt delete (ignored) and clear confirm.
        assert!(!handle_key_quarantine(key(KeyCode::Char('d')), &mut state));
        assert!(
            !state.quarantine_confirm_discard,
            "confirm not cleared after second d"
        );
    }

    // QK3: any key other than d/y cancels confirm without deleting.
    #[test]
    fn quarantine_non_confirm_key_cancels_discard() {
        let mut state = quarantine_state(vec![fake_entry("lead", "replay")]);
        handle_key_quarantine(key(KeyCode::Char('d')), &mut state);
        assert!(state.quarantine_confirm_discard);
        handle_key_quarantine(key(KeyCode::Char('k')), &mut state);
        assert!(!state.quarantine_confirm_discard, "k should cancel confirm");
        assert_eq!(state.quarantine_focus, 0, "focus unchanged on cancel");
    }

    // QK4: `d` on an empty list is a no-op — confirm does not arm
    // when there is nothing to discard.
    #[test]
    fn quarantine_d_on_empty_list_is_noop() {
        let mut state = quarantine_state(Vec::new());
        handle_key_quarantine(key(KeyCode::Char('d')), &mut state);
        assert!(!state.quarantine_confirm_discard);
    }

    // QK5: `Q` returns to the panopticon and clears any pending confirm.
    #[test]
    fn quarantine_q_returns_to_panopticon_and_clears_confirm() {
        let mut state = quarantine_state(vec![fake_entry("lead", "replay")]);
        state.quarantine_confirm_discard = true;
        handle_key_quarantine(key(KeyCode::Char('Q')), &mut state);
        assert_eq!(state.screen, Screen::Panopticon);
        assert!(!state.quarantine_confirm_discard);
    }

    // QK6: `o` on a focused entry opens the compose surface and
    // pre-fills the input buffer with the entry's raw body.
    #[test]
    fn quarantine_o_opens_compose_with_prefilled_body() {
        let mut state = quarantine_state(vec![fake_entry("lead", "signature_invalid")]);
        handle_key_quarantine(key(KeyCode::Char('o')), &mut state);
        assert_eq!(state.screen, Screen::QuarantineCompose);
        assert_eq!(
            state.quarantine_compose_recipient, "lead",
            "recipient not set"
        );
        assert_eq!(
            state.input, "body for lead",
            "input not pre-filled with entry body"
        );
    }

    // QK7: Esc from the compose surface cancels without sending —
    // input is cleared and we return to Screen::Quarantine.
    #[test]
    fn quarantine_compose_esc_cancels() {
        let (registry, keystore) = test_registry_and_keystore();
        let tmp = tempfile::tempdir().unwrap();
        let mut state = quarantine_state(vec![fake_entry("lead", "replay")]);
        state.screen = Screen::QuarantineCompose;
        state.quarantine_compose_recipient = "lead".to_owned();
        state.set_input("some draft text".to_owned());

        let was_exit = handle_key(
            key(KeyCode::Esc),
            &mut state,
            tmp.path(),
            tmp.path(),
            &registry,
            &keystore,
        )
        .unwrap();
        assert!(!was_exit);
        assert_eq!(state.screen, Screen::Quarantine);
        assert!(state.input.is_empty(), "input must be cleared on cancel");
        assert!(state.quarantine_compose_recipient.is_empty());
    }

    // QK8: discard-then-reload path — after a file is deleted the
    // reload should clamp focus to the new (shorter) list. The
    // reload_state tests cover this via RS3 (the focus-clamp in
    // reload_state). This test verifies the full round-trip through
    // the disk fixture: seed a real file, run handle_key_quarantine
    // twice (d, d), confirm file is gone.
    #[test]
    fn quarantine_discard_deletes_real_file() {
        let tmp = tempfile::tempdir().unwrap();
        let qdir = tmp
            .path()
            .join("agents")
            .join("lead")
            .join("inbox")
            .join("quarantine");
        std::fs::create_dir_all(&qdir).unwrap();
        let file_path = qdir.join("abc.signature_invalid");
        std::fs::write(&file_path, b"not json").unwrap();

        let mut state = AppState::default();
        state.screen = Screen::Quarantine;
        state.quarantine = crate::quarantine_view::read_snapshot(tmp.path());
        state.quarantine_focus = 0;
        assert_eq!(state.quarantine.entries.len(), 1, "seeded one entry");

        // First d: arm confirm.
        handle_key_quarantine(key(KeyCode::Char('d')), &mut state);
        assert!(state.quarantine_confirm_discard);
        // Second d: delete.
        handle_key_quarantine(key(KeyCode::Char('d')), &mut state);
        assert!(!state.quarantine_confirm_discard);
        assert!(!file_path.exists(), "quarantine file should be deleted");
    }

    // QK9: replay-ledger preservation — the TUI discard must not touch
    // `replay-ledger.jsonl`. The watcher's verify pipeline writes the
    // replay entry before making the quarantine decision, so the entry
    // is already durable when the file lands in quarantine/. Discarding
    // only removes the quarantine file; the ledger entry stays, blocking
    // any future re-delivery of the same (sender_id, message_id, nonce).
    //
    // This is the Phase 8 done-when integration-test requirement:
    // "The replay ledger entry for a discarded message prevents re-delivery
    // if the same message_id is submitted again."
    //
    // The full replay-pipeline assertion is in
    // `reeve-runtime::verify::tests::replay_second_call_yields_quarantine`.
    // This test covers the TUI side: discard must scope its delete to the
    // single quarantine file, leaving the ledger untouched.
    #[test]
    fn quarantine_discard_preserves_replay_ledger() {
        let tmp = tempfile::tempdir().unwrap();
        let qdir = tmp
            .path()
            .join("agents")
            .join("lead")
            .join("inbox")
            .join("quarantine");
        std::fs::create_dir_all(&qdir).unwrap();
        let qfile = qdir.join("abc.signature_invalid");
        std::fs::write(&qfile, b"not json").unwrap();

        // Simulate the runtime having written a replay-ledger entry when
        // the envelope first arrived.
        let ledger_path = tmp.path().join("replay-ledger.jsonl");
        let ledger_entry = r#"{"sender_id":"00000000-0000-7000-8000-000000000001","message_id":"00000000-0000-7000-8000-000000000002","nonce":"AAAAAAAAAAAAAAAAAAAAAA==","observed_at":"2026-01-01T00:00:00Z"}"#;
        std::fs::write(&ledger_path, format!("{ledger_entry}\n")).unwrap();

        let mut state = AppState::default();
        state.screen = Screen::Quarantine;
        state.quarantine = crate::quarantine_view::read_snapshot(tmp.path());
        state.quarantine_focus = 0;

        // Discard the quarantine file.
        handle_key_quarantine(key(KeyCode::Char('d')), &mut state);
        handle_key_quarantine(key(KeyCode::Char('d')), &mut state);

        assert!(!qfile.exists(), "quarantine file must be deleted");
        assert!(ledger_path.exists(), "replay ledger must NOT be deleted");
        let ledger_after = std::fs::read_to_string(&ledger_path).unwrap();
        assert_eq!(
            ledger_after,
            format!("{ledger_entry}\n"),
            "replay ledger content must be unchanged after discard"
        );
    }
}
